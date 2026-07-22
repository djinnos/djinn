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
    fn worker_identity(&self) -> Result<WorkerIdentity, Error>;
}

pub struct NativeWorkerInspector;
impl WorkerInspector for NativeWorkerInspector {
    fn worker_identity(&self) -> Result<WorkerIdentity, Error> {
        let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE) };
        if dumpable < 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(WorkerIdentity {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            dumpable: dumpable != 0,
        })
    }
}

pub fn verify_worker_ready(inspector: &impl WorkerInspector) -> Result<(), Error> {
    inspector.worker_identity()?.validate()
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
    // These operations must happen while the broker still holds the privilege
    // necessary to make them irreversible.
    syscalls.set_groups_empty()?;
    syscalls.clear_capabilities()?;
    syscalls.set_gid(ARTIFACT_GID)?;
    syscalls.set_uid(CHILD_UID)?;
    syscalls.set_umask(0o002)?;
    syscalls.set_no_new_privs()?;
    syscalls.install_restricted_seccomp()?;
    Ok(())
}

/// Production implementation for the portions that do not require a runtime
/// seccomp profile. `install_restricted_seccomp` fails closed until the broker
/// injects an audited BPF profile through its syscall seam.
pub struct NativeChildSyscalls;
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
        Err(Error::ChildPreparation(
            "capability clearing requires audited runtime seam",
        ))
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
        Err(Error::SeccompUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Calls(Vec<String>);
    impl ChildSyscalls for Calls {
        fn close(&mut self, fd: RawFd) -> Result<(), Error> {
            self.0.push(format!("close:{fd}"));
            Ok(())
        }
        fn set_groups_empty(&mut self) -> Result<(), Error> {
            self.0.push("groups".into());
            Ok(())
        }
        fn clear_capabilities(&mut self) -> Result<(), Error> {
            self.0.push("caps".into());
            Ok(())
        }
        fn set_gid(&mut self, _: u32) -> Result<(), Error> {
            self.0.push("gid".into());
            Ok(())
        }
        fn set_uid(&mut self, _: u32) -> Result<(), Error> {
            self.0.push("uid".into());
            Ok(())
        }
        fn set_umask(&mut self, _: u32) -> Result<(), Error> {
            self.0.push("umask".into());
            Ok(())
        }
        fn set_no_new_privs(&mut self) -> Result<(), Error> {
            self.0.push("nnp".into());
            Ok(())
        }
        fn install_restricted_seccomp(&mut self) -> Result<(), Error> {
            self.0.push("seccomp".into());
            Ok(())
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
            calls.0,
            [
                "close:9", "close:10", "groups", "caps", "gid", "uid", "umask", "nnp", "seccomp"
            ]
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
        assert!(
            ChildMounts {
                broker_socket: true,
                ..ChildMounts::isolated()
            }
            .validate()
            .is_err()
        );
    }
}
