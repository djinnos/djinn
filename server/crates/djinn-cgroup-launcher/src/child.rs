//! Fail-closed child credential, descriptor, and syscall preparation boundary.

use std::os::fd::RawFd;

use crate::Error;

pub const CHILD_UID: u32 = 1001;
pub const ARTIFACT_GID: u32 = 1000;
pub const WORKER_UID: u32 = 1000;
pub const WORKER_GID: u32 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerIdentity {
    pub uid: u32,
    pub gid: u32,
    pub dumpable: bool,
}

impl WorkerIdentity {
    pub fn validate(self) -> Result<(), Error> {
        (self.uid == WORKER_UID && self.gid == WORKER_GID && !self.dumpable)
            .then_some(())
            .ok_or(Error::InvalidWorker)
    }
}

/// Injected worker inspection seam, permitting readiness tests without /proc.
pub trait WorkerInspector {
    /// Inspect the worker identified by the kernel PID authenticated on the
    /// broker socket. Implementations must not trust caller-supplied fields.
    fn worker_identity(&self, pid: u32) -> Result<WorkerIdentity, Error>;
}

pub struct NativeWorkerInspector;
impl WorkerInspector for NativeWorkerInspector {
    fn worker_identity(&self, pid: u32) -> Result<WorkerIdentity, Error> {
        // UID/GID are kernel-owned procfs fields. Do not accept a
        // caller-supplied readiness assertion here.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let field = |name| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|value| value.split_ascii_whitespace().next())
                .ok_or(Error::InvalidWorker)
        };
        let uid = field("Uid:")?.parse().map_err(|_| Error::InvalidWorker)?;
        let gid = field("Gid:")?.parse().map_err(|_| Error::InvalidWorker)?;
        // Linux does not expose PR_GET_DUMPABLE in procfs (CoreDumping is an
        // in-progress core dump, not the dumpability policy). Opening
        // /proc/<pid>/mem performs a kernel ptrace-access check. Temporarily
        // use the required worker fs credentials so this observes policy
        // instead of relying on root privilege; CAP_SYS_PTRACE would bypass
        // the policy and therefore rejects readiness fail-closed.
        let dumpable = worker_mem_accessible(pid, uid, gid)?;
        Ok(WorkerIdentity { uid, gid, dumpable })
    }
}

fn worker_mem_accessible(pid: u32, uid: u32, gid: u32) -> Result<bool, Error> {
    // linux/capability.h: CAP_SYS_PTRACE is capability 19. libc does not
    // expose capability-number constants on every supported target.
    const CAP_SYS_PTRACE: u32 = 19;
    if has_effective_capability(CAP_SYS_PTRACE)? {
        return Err(Error::InvalidWorker);
    }
    let previous_gid = unsafe { libc::setfsgid(gid) };
    let previous_uid = unsafe { libc::setfsuid(uid) };
    // setfs*id reports the prior value rather than errno. Probe the resulting
    // value before relying on it, then leave the requested value in place for
    // the access check.
    let active_gid = unsafe { libc::setfsgid(gid) };
    let active_uid = unsafe { libc::setfsuid(uid) };
    if active_uid != uid as libc::c_int || active_gid != gid as libc::c_int {
        let _ = unsafe { libc::setfsuid(previous_uid as libc::uid_t) };
        let _ = unsafe { libc::setfsgid(previous_gid as libc::gid_t) };
        return Err(Error::InvalidWorker);
    }
    let opened = std::fs::File::open(format!("/proc/{pid}/mem")).is_ok();
    // Restoration failures leave the broker with unsafe credentials.
    let restored_uid = unsafe { libc::setfsuid(previous_uid as libc::uid_t) } == uid as libc::c_int;
    let restored_gid = unsafe { libc::setfsgid(previous_gid as libc::gid_t) } == gid as libc::c_int;
    if !restored_uid || !restored_gid {
        return Err(Error::InvalidWorker);
    }
    Ok(opened)
}

fn has_effective_capability(capability: u32) -> Result<bool, Error> {
    let header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capget, &raw const header, data.as_mut_ptr()) } != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    let word = usize::try_from(capability / 32).map_err(|_| Error::InvalidWorker)?;
    let bit = capability % 32;
    Ok(data
        .get(word)
        .is_some_and(|capabilities| capabilities.effective & (1 << bit) != 0))
}

pub fn verify_worker_ready(inspector: &impl WorkerInspector, pid: u32) -> Result<(), Error> {
    if pid == 0 {
        return Err(Error::InvalidWorker);
    }
    inspector.worker_identity(pid)?.validate()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorKind {
    Ordinary,
    BrokerSocket,
    BrokerCredential,
    ControlAuthority,
    PrivateCgroupMount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildDescriptor {
    pub fd: RawFd,
    pub kind: DescriptorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildMounts {
    pub broker_socket: bool,
    pub worker_private: bool,
    pub private_cgroup: bool,
    pub control_mount: bool,
}

impl ChildMounts {
    pub fn isolated() -> Self {
        Self {
            broker_socket: false,
            worker_private: false,
            private_cgroup: false,
            control_mount: false,
        }
    }
    pub fn validate(&self) -> Result<(), Error> {
        (!self.broker_socket && !self.worker_private && !self.private_cgroup && !self.control_mount)
            .then_some(())
            .ok_or(Error::ChildIsolationViolation)
    }
}

pub trait ChildSyscalls {
    fn close(&mut self, fd: RawFd) -> Result<(), Error>;
    fn set_groups_empty(&mut self) -> Result<(), Error>;
    fn clear_capabilities(&mut self) -> Result<(), Error>;
    fn set_gid(&mut self, gid: u32) -> Result<(), Error>;
    fn set_uid(&mut self, uid: u32) -> Result<(), Error>;
    fn set_umask(&mut self, mask: u32) -> Result<(), Error>;
    fn set_no_new_privs(&mut self) -> Result<(), Error>;
    fn install_restricted_seccomp(&mut self) -> Result<(), Error>;
}

/// Validate every child-visible resource and apply the irreversible drops before exec.
pub fn prepare_child(
    syscalls: &mut impl ChildSyscalls,
    descriptors: &[ChildDescriptor],
    mounts: &ChildMounts,
) -> Result<(), Error> {
    mounts.validate()?;
    for descriptor in descriptors {
        if descriptor.kind != DescriptorKind::Ordinary {
            syscalls.close(descriptor.fd)?;
        }
    }
    // Keep CAP_SETGID/CAP_SETUID until both IDs have been changed; clearing
    // capabilities before these calls makes the native sequence impossible.
    syscalls.set_groups_empty()?;
    syscalls.set_gid(ARTIFACT_GID)?;
    syscalls.set_uid(CHILD_UID)?;
    syscalls.clear_capabilities()?;
    syscalls.set_umask(0o002)?;
    syscalls.set_no_new_privs()?;
    syscalls.install_restricted_seccomp()?;
    Ok(())
}

/// Production child syscall implementation.
pub struct NativeChildSyscalls;

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

impl ChildSyscalls for NativeChildSyscalls {
    fn close(&mut self, fd: RawFd) -> Result<(), Error> {
        if unsafe { libc::close(fd) } == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }
    fn set_groups_empty(&mut self) -> Result<(), Error> {
        if unsafe { libc::setgroups(0, std::ptr::null()) } == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }
    fn clear_capabilities(&mut self) -> Result<(), Error> {
        let header = CapabilityHeader {
            version: 0x2008_0522,
            pid: 0,
        };
        let data = [CapabilityData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        if unsafe { libc::syscall(libc::SYS_capset, &raw const header, data.as_ptr()) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        if unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        } != 0
        {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }
    fn set_gid(&mut self, gid: u32) -> Result<(), Error> {
        if unsafe { libc::setresgid(gid, gid, gid) } == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }
    fn set_uid(&mut self, uid: u32) -> Result<(), Error> {
        if unsafe { libc::setresuid(uid, uid, uid) } == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }
    fn set_umask(&mut self, mask: u32) -> Result<(), Error> {
        unsafe {
            libc::umask(mask);
        }
        Ok(())
    }
    fn set_no_new_privs(&mut self) -> Result<(), Error> {
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }
    fn install_restricted_seccomp(&mut self) -> Result<(), Error> {
        const RET_ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
        const RET_ERRNO: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
        const LD_NR: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
        // Seccomp remains active after exec. An exec-only allow-list breaks a
        // dynamic loader before ordinary task commands begin. The preparation
        // above has already removed credentials, mounts, cgroups, and control
        // descriptors, so retain ordinary runtime syscalls and deny the
        // remaining namespace, cross-process, and kernel-control surface.
        let denied = [
            libc::SYS_setns as u32,
            libc::SYS_unshare as u32,
            libc::SYS_mount as u32,
            libc::SYS_umount2 as u32,
            libc::SYS_pivot_root as u32,
            libc::SYS_ptrace as u32,
            libc::SYS_process_vm_readv as u32,
            libc::SYS_process_vm_writev as u32,
            libc::SYS_bpf as u32,
            libc::SYS_perf_event_open as u32,
            libc::SYS_kexec_load as u32,
            libc::SYS_init_module as u32,
            libc::SYS_finit_module as u32,
            libc::SYS_delete_module as u32,
            libc::SYS_reboot as u32,
            libc::SYS_swapon as u32,
            libc::SYS_swapoff as u32,
            libc::SYS_keyctl as u32,
            libc::SYS_add_key as u32,
            libc::SYS_request_key as u32,
            libc::SYS_open_by_handle_at as u32,
            libc::SYS_name_to_handle_at as u32,
            libc::SYS_pidfd_getfd as u32,
            libc::SYS_capset as u32,
            libc::SYS_setuid as u32,
            libc::SYS_setgid as u32,
            libc::SYS_setreuid as u32,
            libc::SYS_setregid as u32,
            libc::SYS_setresuid as u32,
            libc::SYS_setresgid as u32,
            libc::SYS_setgroups as u32,
        ];
        let mut filter = Vec::with_capacity(denied.len() * 2 + 2);
        filter.push(libc::sock_filter {
            code: LD_NR,
            jt: 0,
            jf: 0,
            k: 0,
        });
        for syscall in denied {
            filter.push(libc::sock_filter {
                code: JEQ,
                jt: 0,
                jf: 1,
                k: syscall,
            });
            filter.push(libc::sock_filter {
                code: RET,
                jt: 0,
                jf: 0,
                k: RET_ERRNO,
            });
        }
        filter.push(libc::sock_filter {
            code: RET,
            jt: 0,
            jf: 0,
            k: RET_ALLOW,
        });
        let program = libc::sock_fprog {
            len: filter
                .len()
                .try_into()
                .map_err(|_| Error::SeccompUnavailable)?,
            filter: filter.as_mut_ptr(),
        };
        if unsafe {
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &raw const program,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Calls {
        calls: Vec<String>,
        fail_at: Option<&'static str>,
    }
    impl Calls {
        fn record(
            &mut self,
            call: impl Into<String>,
            operation: &'static str,
        ) -> Result<(), Error> {
            self.calls.push(call.into());
            if self.fail_at == Some(operation) {
                Err(Error::ChildPreparation(operation))
            } else {
                Ok(())
            }
        }
    }
    impl ChildSyscalls for Calls {
        fn close(&mut self, fd: RawFd) -> Result<(), Error> {
            self.record(format!("close:{fd}"), "close")
        }
        fn set_groups_empty(&mut self) -> Result<(), Error> {
            self.record("groups", "groups")
        }
        fn clear_capabilities(&mut self) -> Result<(), Error> {
            self.record("caps", "caps")
        }
        fn set_gid(&mut self, _: u32) -> Result<(), Error> {
            self.record("gid", "gid")
        }
        fn set_uid(&mut self, _: u32) -> Result<(), Error> {
            self.record("uid", "uid")
        }
        fn set_umask(&mut self, _: u32) -> Result<(), Error> {
            self.record("umask", "umask")
        }
        fn set_no_new_privs(&mut self) -> Result<(), Error> {
            self.record("nnp", "nnp")
        }
        fn install_restricted_seccomp(&mut self) -> Result<(), Error> {
            self.record("seccomp", "seccomp")
        }
    }
    #[test]
    fn child_drop_order_closes_authority_before_irreversible_credential_drop() {
        let mut calls = Calls::default();
        prepare_child(
            &mut calls,
            &[
                ChildDescriptor {
                    fd: 9,
                    kind: DescriptorKind::BrokerCredential,
                },
                ChildDescriptor {
                    fd: 10,
                    kind: DescriptorKind::ControlAuthority,
                },
            ],
            &ChildMounts::isolated(),
        )
        .unwrap();
        assert_eq!(
            calls.calls,
            [
                "close:9", "close:10", "groups", "gid", "uid", "caps", "umask", "nnp", "seccomp"
            ]
        );
    }
    #[test]
    fn child_preparation_stops_at_the_first_syscall_failure() {
        let mut calls = Calls {
            fail_at: Some("uid"),
            ..Calls::default()
        };
        let error = prepare_child(
            &mut calls,
            &[ChildDescriptor {
                fd: 9,
                kind: DescriptorKind::BrokerSocket,
            }],
            &ChildMounts::isolated(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::ChildPreparation("uid")));
        assert_eq!(calls.calls, ["close:9", "groups", "gid", "uid"]);
    }
    #[test]
    fn every_protected_descriptor_is_closed_before_child_credentials_drop() {
        let mut calls = Calls::default();
        prepare_child(
            &mut calls,
            &[
                ChildDescriptor {
                    fd: 9,
                    kind: DescriptorKind::BrokerSocket,
                },
                ChildDescriptor {
                    fd: 10,
                    kind: DescriptorKind::BrokerCredential,
                },
                ChildDescriptor {
                    fd: 11,
                    kind: DescriptorKind::ControlAuthority,
                },
                ChildDescriptor {
                    fd: 12,
                    kind: DescriptorKind::PrivateCgroupMount,
                },
            ],
            &ChildMounts::isolated(),
        )
        .unwrap();
        assert_eq!(
            &calls.calls[..4],
            ["close:9", "close:10", "close:11", "close:12"]
        );
    }
    #[test]
    fn dumpable_or_wrongly_owned_worker_and_visible_private_mounts_fail_closed() {
        assert!(
            WorkerIdentity {
                uid: 1000,
                gid: 1000,
                dumpable: true
            }
            .validate()
            .is_err()
        );
        assert!(
            WorkerIdentity {
                uid: 1001,
                gid: 1000,
                dumpable: false
            }
            .validate()
            .is_err()
        );
        for mounts in [
            ChildMounts {
                broker_socket: true,
                ..ChildMounts::isolated()
            },
            ChildMounts {
                worker_private: true,
                ..ChildMounts::isolated()
            },
            ChildMounts {
                private_cgroup: true,
                ..ChildMounts::isolated()
            },
            ChildMounts {
                control_mount: true,
                ..ChildMounts::isolated()
            },
        ] {
            assert!(mounts.validate().is_err());
        }
    }
}
