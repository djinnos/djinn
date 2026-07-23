//! Authenticated, invocation-bound controls for the privileged launcher.
//!
//! This module deliberately has no supervisor dependency.  A non-dumpable
//! worker obtains lease state elsewhere and presents only the resulting fence.

use std::{collections::HashMap, fs::File, io::Read, os::fd::RawFd};

use crate::{
<<<<<<< HEAD
    CgroupFs, CloneIntoCgroup, CpuStat, Error, Invocation, Launcher, Leaf,
    child::WorkerReadinessAssertion,
=======
    CgroupFs, ChildProcess, CloneIntoCgroup, CommandSpec, CpuStat, Error, Invocation, Launcher,
    Leaf, child::WorkerReadinessAssertion,
>>>>>>> origin/main
};

pub const WORKER_UID: u32 = 1000;
pub const WORKER_GID: u32 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixPeer {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Injectable source for `SO_PEERCRED`; production uses [`SocketPeer`].
pub trait PeerCredentials {
    fn peer_credentials(&self) -> Result<UnixPeer, Error>;
}

pub struct SocketPeer(pub RawFd);
impl PeerCredentials for SocketPeer {
    fn peer_credentials(&self) -> Result<UnixPeer, Error> {
        let mut credential = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                self.0,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credential).cast(),
                &mut length,
            )
        };
        if rc != 0 || length != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
            return Err(Error::UnauthenticatedPeer);
        }
        Ok(UnixPeer {
            pid: credential
                .pid
                .try_into()
                .map_err(|_| Error::UnauthenticatedPeer)?,
            uid: credential.uid,
            gid: credential.gid,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ConnectionId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ControlNonce([u8; 32]);
impl ControlNonce {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

pub trait NonceSource {
    fn nonce(&mut self) -> Result<ControlNonce, Error>;
}

/// Kernel-backed nonce source. Failure to read entropy rejects the control.
pub struct OsNonceSource;
impl NonceSource for OsNonceSource {
    fn nonce(&mut self) -> Result<ControlNonce, Error> {
        let mut bytes = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(Error::Io)?;
        Ok(ControlNonce(bytes))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerConfig {
    pub worker_pid: u32,
    pub worker_uid: u32,
    pub worker_gid: u32,
    /// A per-pod secret held only in the worker-private mount.
    pub pod_credential: Vec<u8>,
}

impl BrokerConfig {
    pub fn worker(worker_pid: u32, pod_credential: Vec<u8>) -> Result<Self, Error> {
        if worker_pid == 0 || pod_credential.is_empty() {
            return Err(Error::InvalidWorker);
        }
        Ok(Self {
            worker_pid,
            worker_uid: WORKER_UID,
            worker_gid: WORKER_GID,
            pod_credential,
        })
    }
}

struct ActiveInvocation {
    connection: ConnectionId,
    invocation: Invocation,
    nonce: ControlNonce,
    leaf: Option<Leaf>,
    child: Option<ChildProcess>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<ChildStatus>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStatus {
    Running,
    Exited(u8),
    Signaled(u8),
}
const OUTPUT_LIMIT: usize = 32 * 1024;

struct WorkerConnection {
    _peer: UnixPeer,
    ready: bool,
}

/// The broker has no coordinator/service capability. All state is scoped to an
/// authenticated connection and one broker-assigned invocation binding.
pub struct Broker<F, S, N = OsNonceSource> {
    launcher: Launcher<F, S>,
    config: BrokerConfig,
    nonces: N,
    next_connection: u64,
    connections: HashMap<ConnectionId, WorkerConnection>,
    active: HashMap<String, ActiveInvocation>,
}

impl<F: CgroupFs, S: CloneIntoCgroup, N: NonceSource> Broker<F, S, N> {
    pub fn new(launcher: Launcher<F, S>, config: BrokerConfig, nonces: N) -> Result<Self, Error> {
        Ok(Self {
            launcher,
            config,
            nonces,
            next_connection: 0,
            connections: HashMap::new(),
            active: HashMap::new(),
        })
    }

    /// Authenticate the socket peer and private credential before allocating a connection.
    pub fn authenticate<P: PeerCredentials>(
        &mut self,
        peer: &P,
        credential: &[u8],
    ) -> Result<ConnectionId, Error> {
        let actual = peer.peer_credentials()?;
        if actual.pid != self.config.worker_pid
            || actual.uid != self.config.worker_uid
            || actual.gid != self.config.worker_gid
        {
            return Err(Error::UnauthenticatedPeer);
        }
        if credential != self.config.pod_credential.as_slice() {
            return Err(Error::InvalidCredential);
        }
        self.next_connection = self
            .next_connection
            .checked_add(1)
            .ok_or(Error::InvalidControl)?;
        let id = ConnectionId(self.next_connection);
        self.connections.insert(
            id,
            WorkerConnection {
                _peer: actual,
                ready: false,
            },
        );
        Ok(id)
    }

    /// Accept readiness only on a connection whose peer PID/UID and
    /// worker-private credential have already been authenticated.
    pub fn accept_worker_readiness(
        &mut self,
        connection: ConnectionId,
        _assertion: WorkerReadinessAssertion,
    ) -> Result<(), Error> {
        self.connections
            .get_mut(&connection)
            .ok_or(Error::UnauthenticatedPeer)?
            .ready = true;
        Ok(())
    }

    /// Bind a fresh broker invocation to one authenticated worker connection.
    pub fn begin_invocation(
        &mut self,
        connection: ConnectionId,
        invocation: Invocation,
    ) -> Result<ControlNonce, Error> {
        self.require_ready_connection(connection)?;
        if self.active.contains_key(&invocation.id) {
            return Err(Error::InvalidInvocationBinding);
        }
        let nonce = self.nonces.nonce()?;
        self.active.insert(
            invocation.id.clone(),
            ActiveInvocation {
                connection,
                invocation,
                nonce,
                leaf: None,
                child: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: None,
            },
        );
        Ok(nonce)
    }

    pub fn create(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
        leaf_name: &str,
        command: &CommandSpec,
    ) -> Result<ControlNonce, Error> {
        self.validate(connection, id, nonce)?;
        // Reject a duplicate before it can create a second cgroup or clone a
        // second child into it.
        if self
            .active
            .get(id)
            .ok_or(Error::InvalidInvocationBinding)?
            .leaf
            .is_some()
        {
            return Err(Error::InvalidControl);
        }
        let invocation = self
            .active
            .get(id)
            .ok_or(Error::InvalidInvocationBinding)?
            .invocation
            .clone();
        command.validate()?;
        let (leaf, child) = self
            .launcher
            .create_command(leaf_name, invocation, command)?;
        let active = self
            .active
            .get_mut(id)
            .ok_or(Error::InvalidInvocationBinding)?;
        active.leaf = Some(leaf);
        active.child = Some(child);
        self.rotate(id)
    }

    pub fn output(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
        stderr: bool,
    ) -> Result<(Vec<u8>, bool, ChildStatus, ControlNonce), Error> {
        self.validate(connection, id, nonce)?;
        let active = self
            .active
            .get_mut(id)
            .ok_or(Error::InvalidInvocationBinding)?;
        let child = active.child.ok_or(Error::InvalidControl)?;
        let (fd, target) = if stderr {
            (child.stderr, &mut active.stderr)
        } else {
            (child.stdout, &mut active.stdout)
        };
        let mut buf = [0_u8; 4096];
        let count = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        let eof = if count == 0 {
            true
        } else if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                false
            } else {
                return Err(Error::Io(error));
            }
        } else {
            if target.len().saturating_add(count as usize) > OUTPUT_LIMIT {
                return Err(Error::InvalidControl);
            }
            target.extend_from_slice(&buf[..count as usize]);
            false
        };
        let status = match active.status {
            Some(value) => value,
            None => {
                let mut raw = 0;
                let waited = unsafe { libc::waitpid(child.pid, &mut raw, libc::WNOHANG) };
                let value = if waited == 0 {
                    ChildStatus::Running
                } else if waited < 0 {
                    return Err(Error::InvalidChild);
                } else if libc::WIFEXITED(raw) {
                    ChildStatus::Exited(libc::WEXITSTATUS(raw) as u8)
                } else {
                    ChildStatus::Signaled(libc::WTERMSIG(raw) as u8)
                };
                if value != ChildStatus::Running {
                    active.status = Some(value);
                }
                value
            }
        };
        let data = std::mem::take(target);
        let nonce = self.rotate(id)?;
        Ok((data, eof, status, nonce))
    }

    pub fn sample(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
    ) -> Result<(CpuStat, ControlNonce), Error> {
        self.validate(connection, id, nonce)?;
        let (launcher, active) = (
            &mut self.launcher,
            self.active
                .get_mut(id)
                .ok_or(Error::InvalidInvocationBinding)?,
        );
        let sample = launcher.sample(active.leaf.as_ref().ok_or(Error::InvalidControl)?)?;
        let nonce = self.rotate(id)?;
        Ok((sample, nonce))
    }

    pub fn lift(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
        fence: u64,
    ) -> Result<ControlNonce, Error> {
        self.validate(connection, id, nonce)?;
        let (launcher, active) = (
            &mut self.launcher,
            self.active
                .get_mut(id)
                .ok_or(Error::InvalidInvocationBinding)?,
        );
        launcher.fenced_lift(active.leaf.as_mut().ok_or(Error::InvalidControl)?, fence)?;
        self.rotate(id)
    }

    pub fn kill(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
    ) -> Result<ControlNonce, Error> {
        self.validate(connection, id, nonce)?;
        let (launcher, active) = (
            &mut self.launcher,
            self.active
                .get_mut(id)
                .ok_or(Error::InvalidInvocationBinding)?,
        );
        launcher.kill(active.leaf.as_mut().ok_or(Error::InvalidControl)?)?;
        self.rotate(id)
    }

    pub fn cleanup(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
    ) -> Result<(), Error> {
        self.validate(connection, id, nonce)?;
        let active = self.active.get(id).ok_or(Error::InvalidInvocationBinding)?;
        self.launcher
            .remove(active.leaf.as_ref().ok_or(Error::InvalidControl)?)?;
        // A failed removal is retryable with the same nonce and binding.
        let active = self
            .active
            .remove(id)
            .ok_or(Error::InvalidInvocationBinding)?;
        if let Some(child) = active.child {
            for fd in [child.stdout, child.stderr] {
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                }
            }
        }
        Ok(())
    }

    pub fn wait_empty(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
    ) -> Result<ControlNonce, Error> {
        self.validate(connection, id, nonce)?;
        let (launcher, active) = (
            &mut self.launcher,
            self.active
                .get_mut(id)
                .ok_or(Error::InvalidInvocationBinding)?,
        );
        launcher.wait_empty(active.leaf.as_ref().ok_or(Error::InvalidControl)?)?;
        self.rotate(id)
    }

    fn require_ready_connection(&self, connection: ConnectionId) -> Result<(), Error> {
        self.connections
            .get(&connection)
            .ok_or(Error::UnauthenticatedPeer)?
            .ready
            .then_some(())
            .ok_or(Error::InvalidWorker)
    }
    /// Expected operation failures do not consume a nonce.
    fn validate(
        &mut self,
        connection: ConnectionId,
        id: &str,
        supplied: ControlNonce,
    ) -> Result<(), Error> {
        self.require_ready_connection(connection)?;
        let active = self.active.get(id).ok_or(Error::InvalidInvocationBinding)?;
        if active.connection != connection {
            return Err(Error::InvalidInvocationBinding);
        }
        if active.nonce != supplied {
            return Err(Error::InvalidNonce);
        }
        Ok(())
    }
    fn rotate(&mut self, id: &str) -> Result<ControlNonce, Error> {
        let replacement = self.nonces.nonce()?;
        self.active
            .get_mut(id)
            .ok_or(Error::InvalidInvocationBinding)?
            .nonce = replacement;
        Ok(replacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::child::{WorkerDumpability, prepare_worker_readiness};
    use std::{
        cell::Cell,
        collections::{BTreeSet, HashMap},
        rc::Rc,
    };
    struct Fs {
        files: HashMap<(RawFd, String), String>,
        next: RawFd,
        events: Rc<std::cell::RefCell<String>>,
        creates: Rc<Cell<usize>>,
    }
    impl Fs {
        fn ready() -> Self {
            Self {
                files: HashMap::new(),
                next: 10,
                events: Rc::new(std::cell::RefCell::new("populated 0".into())),
                creates: Rc::new(Cell::new(0)),
            }
        }
    }
    impl CgroupFs for Fs {
        fn readiness(&self) -> Result<crate::Readiness, Error> {
            Ok(crate::Readiness {
                mode: crate::CgroupMode::V2,
                root_writable: true,
                owner_uid: 0,
                delegated_controllers: BTreeSet::from(["cpu".into()]),
            })
        }
        fn create_direct_child(&mut self, _: &str) -> Result<RawFd, Error> {
            self.next += 1;
            self.creates.set(self.creates.get() + 1);
            Ok(self.next)
        }
        fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), Error> {
            self.files.insert((fd, file.into()), value.into());
            Ok(())
        }
        fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, Error> {
            if file == "cgroup.events" {
                return Ok(self.events.borrow().clone());
            }
            Ok(self
                .files
                .get(&(fd, file.into()))
                .cloned()
                .unwrap_or_else(|| "usage_usec 0".into()))
        }
        fn remove_leaf(&mut self, _: RawFd, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }
    struct Clone;
    impl CloneIntoCgroup for Clone {
        fn clone_into_cgroup(
            &mut self,
            _: RawFd,
            _: &Invocation,
            _: &crate::CommandSpec,
        ) -> Result<crate::ChildProcess, Error> {
            Ok(crate::ChildProcess {
                pid: 1,
                stdout: -1,
                stderr: -1,
            })
        }
    }
    fn broker() -> Broker<Fs, Clone, Nonces> {
        let launcher = Launcher::new(
            Fs::ready(),
            Clone,
            crate::LauncherConfig::new(None, 0).unwrap(),
        )
        .unwrap();
        Broker::new(
            launcher,
            BrokerConfig::worker(42, b"private".to_vec()).unwrap(),
            Nonces(0),
        )
        .unwrap()
    }
    struct ReadyDumpability;
    impl WorkerDumpability for ReadyDumpability {
        fn set_non_dumpable(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn get_dumpable(&mut self) -> Result<i32, Error> {
            Ok(0)
        }
    }
    fn readiness() -> WorkerReadinessAssertion {
        prepare_worker_readiness(&mut ReadyDumpability).unwrap()
    }
    fn command() -> CommandSpec {
        CommandSpec {
            program: "/bin/true".into(),
            argv: vec![],
            cwd: "/workspace".into(),
            environment: vec![],
        }
    }
    #[derive(Clone, Copy)]
    struct FakePeer(UnixPeer);
    impl PeerCredentials for FakePeer {
        fn peer_credentials(&self) -> Result<UnixPeer, Error> {
            Ok(self.0)
        }
    }
    struct Nonces(u8);
    impl NonceSource for Nonces {
        fn nonce(&mut self) -> Result<ControlNonce, Error> {
            self.0 += 1;
            Ok(ControlNonce([self.0; 32]))
        }
    }

    #[test]
    fn peer_authentication_rejects_children_siblings_and_bad_credentials() {
        let mut broker = broker();
        let good = FakePeer(UnixPeer {
            pid: 42,
            uid: 1000,
            gid: 1000,
        });
        let child = FakePeer(UnixPeer {
            pid: 43,
            uid: 1001,
            gid: 1000,
        });
        let sibling = FakePeer(UnixPeer {
            pid: 44,
            uid: 1000,
            gid: 1000,
        });
        assert!(matches!(
            broker.authenticate(&child, b"private"),
            Err(Error::UnauthenticatedPeer)
        ));
        assert!(matches!(
            broker.authenticate(&sibling, b"private"),
            Err(Error::UnauthenticatedPeer)
        ));
        assert!(matches!(
            broker.authenticate(&good, b"forged"),
            Err(Error::InvalidCredential)
        ));
        assert!(broker.authenticate(&good, b"private").is_ok());
    }

    #[test]
    fn stale_bindings_forged_fences_and_replayed_controls_are_rejected() {
        let mut broker = broker();
        let peer = FakePeer(UnixPeer {
            pid: 42,
            uid: 1000,
            gid: 1000,
        });
        let connection = broker.authenticate(&peer, b"private").unwrap();
        broker
            .accept_worker_readiness(connection, readiness())
            .unwrap();
        let nonce = broker
            .begin_invocation(
                connection,
                Invocation {
                    id: "one".into(),
                    fence: 9,
                },
            )
            .unwrap();
        assert!(matches!(
            broker.create(connection, "other", nonce, "leaf", &command()),
            Err(Error::InvalidInvocationBinding)
        ));
        let stale = nonce;
        let nonce = broker
            .create(connection, "one", nonce, "leaf", &command())
            .unwrap();
        // The old create nonce is now irreversibly stale and cannot replay a control.
        assert!(matches!(
            broker.lift(connection, "one", stale, 9),
            Err(Error::InvalidNonce)
        ));
        assert!(matches!(
            broker.lift(connection, "one", nonce, 8),
            Err(Error::FenceMismatch)
        ));
        // A rejected fence leaves the authenticated nonce usable for a retry.
        assert!(broker.lift(connection, "one", nonce, 9).is_ok());
    }

    #[test]
    fn missing_worker_readiness_assertion_fails_closed() {
        let mut broker = broker();
        let peer = FakePeer(UnixPeer {
            pid: 42,
            uid: 1000,
            gid: 1000,
        });
        let connection = broker.authenticate(&peer, b"private").unwrap();
        assert!(matches!(
            broker.begin_invocation(
                connection,
                Invocation {
                    id: "one".into(),
                    fence: 9
                }
            ),
            Err(Error::InvalidWorker)
        ));
        broker
            .accept_worker_readiness(connection, readiness())
            .unwrap();
        assert!(
            broker
                .begin_invocation(
                    connection,
                    Invocation {
                        id: "one".into(),
                        fence: 9
                    }
                )
                .is_ok()
        );
    }

    #[test]
    fn duplicate_create_does_not_clone_or_replace_the_active_leaf() {
        let fs = Fs::ready();
        let creates = Rc::clone(&fs.creates);
        let launcher =
            Launcher::new(fs, Clone, crate::LauncherConfig::new(None, 0).unwrap()).unwrap();
        let mut broker = Broker::new(
            launcher,
            BrokerConfig::worker(42, b"private".to_vec()).unwrap(),
            Nonces(0),
        )
        .unwrap();
        let peer = FakePeer(UnixPeer {
            pid: 42,
            uid: 1000,
            gid: 1000,
        });
        let connection = broker.authenticate(&peer, b"private").unwrap();
        broker
            .accept_worker_readiness(connection, readiness())
            .unwrap();
        let nonce = broker
            .begin_invocation(
                connection,
                Invocation {
                    id: "one".into(),
                    fence: 9,
                },
            )
            .unwrap();
        let nonce = broker
            .create(connection, "one", nonce, "first", &command())
            .unwrap();
        assert_eq!(creates.get(), 1);
        assert!(matches!(
            broker.create(connection, "one", nonce, "second", &command()),
            Err(Error::InvalidControl)
        ));
        assert_eq!(creates.get(), 1);
        // The rejected operation leaves both the nonce and leaf binding intact.
        assert!(broker.sample(connection, "one", nonce).is_ok());
    }

    #[test]
    fn failed_cleanup_retains_binding_for_retry() {
        let fs = Fs::ready();
        let events = Rc::clone(&fs.events);
        *events.borrow_mut() = "populated 1".into();
        let launcher =
            Launcher::new(fs, Clone, crate::LauncherConfig::new(None, 0).unwrap()).unwrap();
        let mut broker = Broker::new(
            launcher,
            BrokerConfig::worker(42, b"private".to_vec()).unwrap(),
            Nonces(0),
        )
        .unwrap();
        let peer = FakePeer(UnixPeer {
            pid: 42,
            uid: 1000,
            gid: 1000,
        });
        let connection = broker.authenticate(&peer, b"private").unwrap();
        broker
            .accept_worker_readiness(connection, readiness())
            .unwrap();
        let nonce = broker
            .begin_invocation(
                connection,
                Invocation {
                    id: "one".into(),
                    fence: 9,
                },
            )
            .unwrap();
        let nonce = broker
            .create(connection, "one", nonce, "leaf", &command())
            .unwrap();
        assert!(matches!(
            broker.cleanup(connection, "one", nonce),
            Err(Error::StillPopulated)
        ));
        *events.borrow_mut() = "populated 0".into();
        assert!(broker.cleanup(connection, "one", nonce).is_ok());
    }
}
