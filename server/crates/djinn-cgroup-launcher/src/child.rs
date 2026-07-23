//! Fail-closed child credential, descriptor, and syscall preparation boundary.

use std::os::fd::RawFd;

use crate::Error;

pub const CHILD_UID: u32 = 1001;
pub const ARTIFACT_GID: u32 = 1000;
/// Assertion emitted by the trusted worker only after it locally disabled and
/// re-read its own dumpability state. It is accepted only on an already
/// authenticated worker connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerReadinessAssertion {
    _private: (),
}

/// Injectable worker-local `prctl` seam. Procfs and ptrace access are not
/// sound oracles for another process's dumpability state.
pub trait WorkerDumpability {
    fn set_non_dumpable(&mut self) -> Result<(), Error>;
    fn get_dumpable(&mut self) -> Result<i32, Error>;
}

pub struct NativeWorkerDumpability;
impl WorkerDumpability for NativeWorkerDumpability {
    fn set_non_dumpable(&mut self) -> Result<(), Error> {
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }

    fn get_dumpable(&mut self) -> Result<i32, Error> {
        let result = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
        if result < 0 {
            Err(Error::Io(std::io::Error::last_os_error()))
        } else {
            Ok(result)
        }
    }
}

/// Run this in the worker process before sending readiness to the broker.
pub fn prepare_worker_readiness(
    syscalls: &mut impl WorkerDumpability,
) -> Result<WorkerReadinessAssertion, Error> {
    syscalls.set_non_dumpable()?;
    if syscalls.get_dumpable()? != 0 {
        return Err(Error::InvalidWorker);
    }
    Ok(WorkerReadinessAssertion { _private: () })
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
    struct Dumpability {
        set_ok: bool,
        get_result: Result<i32, Error>,
        calls: Vec<&'static str>,
    }
    impl WorkerDumpability for Dumpability {
        fn set_non_dumpable(&mut self) -> Result<(), Error> {
            self.calls.push("set");
            self.set_ok
                .then_some(())
                .ok_or(Error::ChildPreparation("set_dumpable"))
        }
        fn get_dumpable(&mut self) -> Result<i32, Error> {
            self.calls.push("get");
            self.get_result
                .as_ref()
                .copied()
                .map_err(|_| Error::ChildPreparation("get_dumpable"))
        }
    }

    #[test]
    fn worker_readiness_sets_then_locally_confirms_non_dumpability() {
        let mut dumpability = Dumpability {
            set_ok: true,
            get_result: Ok(0),
            calls: vec![],
        };
        assert!(prepare_worker_readiness(&mut dumpability).is_ok());
        assert_eq!(dumpability.calls, ["set", "get"]);
    }

    #[test]
    fn worker_readiness_fails_closed_on_set_get_or_nonzero_result() {
        let cases = [
            (false, Ok(0)),
            (true, Err(Error::InvalidWorker)),
            (true, Ok(1)),
        ];
        for (set_ok, get_result) in cases {
            let mut dumpability = Dumpability {
                set_ok,
                get_result,
                calls: vec![],
            };
            assert!(prepare_worker_readiness(&mut dumpability).is_err());
            assert_eq!(
                dumpability.calls,
                if set_ok {
                    vec!["set", "get"]
                } else {
                    vec!["set"]
                }
            );
        }
    }

    #[test]
    fn visible_private_mounts_fail_closed() {
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
