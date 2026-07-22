//! Authenticated, invocation-bound controls for the privileged launcher.
//!
//! This module deliberately has no supervisor dependency.  A non-dumpable
//! worker obtains lease state elsewhere and presents only the resulting fence.

use std::{collections::HashMap, fs::File, io::Read, os::fd::RawFd};

use crate::{
    CgroupFs, CloneIntoCgroup, CpuStat, Error, Invocation, Launcher, Leaf,
    child::{NativeWorkerInspector, WorkerInspector, verify_worker_ready},
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
}

/// The broker has no coordinator/service capability. All state is scoped to an
/// authenticated connection and one broker-assigned invocation binding.
pub struct Broker<F, S, N = OsNonceSource, I = NativeWorkerInspector> {
    launcher: Launcher<F, S>,
    config: BrokerConfig,
    nonces: N,
    inspector: I,
    next_connection: u64,
    connections: HashMap<ConnectionId, UnixPeer>,
    active: HashMap<String, ActiveInvocation>,
}

impl<F: CgroupFs, S: CloneIntoCgroup, N: NonceSource, I: WorkerInspector> Broker<F, S, N, I> {
    pub fn new(
        launcher: Launcher<F, S>,
        config: BrokerConfig,
        nonces: N,
        inspector: I,
    ) -> Result<Self, Error> {
        verify_worker_ready(&inspector, config.worker_pid)?;
        Ok(Self {
            launcher,
            config,
            nonces,
            inspector,
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
        self.verify_worker_ready()?;
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
        self.connections.insert(id, actual);
        Ok(id)
    }

    /// Bind a fresh broker invocation to one authenticated worker connection.
    pub fn begin_invocation(
        &mut self,
        connection: ConnectionId,
        invocation: Invocation,
    ) -> Result<ControlNonce, Error> {
        self.require_connection(connection)?;
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
    ) -> Result<ControlNonce, Error> {
        self.verify_worker_ready()?;
        self.validate_and_rotate(connection, id, nonce)?;
        let invocation = self
            .active
            .get(id)
            .ok_or(Error::InvalidInvocationBinding)?
            .invocation
            .clone();
        let leaf = self.launcher.create(leaf_name, invocation)?;
        let active = self
            .active
            .get_mut(id)
            .ok_or(Error::InvalidInvocationBinding)?;
        if active.leaf.replace(leaf).is_some() {
            return Err(Error::InvalidControl);
        }
        Ok(active.nonce)
    }

    pub fn sample(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
    ) -> Result<(CpuStat, ControlNonce), Error> {
        self.validate_and_rotate(connection, id, nonce)?;
        let (launcher, active) = (
            &mut self.launcher,
            self.active
                .get_mut(id)
                .ok_or(Error::InvalidInvocationBinding)?,
        );
        let sample = launcher.sample(active.leaf.as_ref().ok_or(Error::InvalidControl)?)?;
        Ok((sample, active.nonce))
    }

    pub fn lift(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
        fence: u64,
    ) -> Result<ControlNonce, Error> {
        self.validate_and_rotate(connection, id, nonce)?;
        let (launcher, active) = (
            &mut self.launcher,
            self.active
                .get_mut(id)
                .ok_or(Error::InvalidInvocationBinding)?,
        );
        launcher.fenced_lift(active.leaf.as_mut().ok_or(Error::InvalidControl)?, fence)?;
        Ok(active.nonce)
    }

    pub fn kill(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
    ) -> Result<ControlNonce, Error> {
        self.validate_and_rotate(connection, id, nonce)?;
        let (launcher, active) = (
            &mut self.launcher,
            self.active
                .get_mut(id)
                .ok_or(Error::InvalidInvocationBinding)?,
        );
        launcher.kill(active.leaf.as_mut().ok_or(Error::InvalidControl)?)?;
        Ok(active.nonce)
    }

    pub fn cleanup(
        &mut self,
        connection: ConnectionId,
        id: &str,
        nonce: ControlNonce,
    ) -> Result<(), Error> {
        self.validate_and_rotate(connection, id, nonce)?;
        let active = self
            .active
            .remove(id)
            .ok_or(Error::InvalidInvocationBinding)?;
        self.launcher
            .remove(active.leaf.as_ref().ok_or(Error::InvalidControl)?)
    }

    fn require_connection(&self, connection: ConnectionId) -> Result<(), Error> {
        self.connections
            .contains_key(&connection)
            .then_some(())
            .ok_or(Error::UnauthenticatedPeer)
    }
    fn verify_worker_ready(&self) -> Result<(), Error> {
        verify_worker_ready(&self.inspector, self.config.worker_pid)
    }
    fn validate_and_rotate(
        &mut self,
        connection: ConnectionId,
        id: &str,
        supplied: ControlNonce,
    ) -> Result<(), Error> {
        self.require_connection(connection)?;
        let active = self.active.get(id).ok_or(Error::InvalidInvocationBinding)?;
        if active.connection != connection {
            return Err(Error::InvalidInvocationBinding);
        }
        if active.nonce != supplied {
            return Err(Error::InvalidNonce);
        }
        let replacement = self.nonces.nonce()?;
        self.active
            .get_mut(id)
            .ok_or(Error::InvalidInvocationBinding)?
            .nonce = replacement;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::child::WorkerIdentity;
    use std::collections::{BTreeSet, HashMap};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Fs {
        files: HashMap<(RawFd, String), String>,
        next: RawFd,
    }
    impl Fs {
        fn ready() -> Self {
            Self {
                files: HashMap::new(),
                next: 10,
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
                .unwrap_or_else(|| "usage_usec 0".into()))
        }
        fn remove_leaf(&mut self, _: RawFd, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }
    struct Clone;
    impl CloneIntoCgroup for Clone {
        fn clone_into_cgroup(&mut self, _: RawFd, _: &Invocation) -> Result<(), Error> {
            Ok(())
        }
    }
    #[derive(Clone)]
    struct Inspector(Arc<AtomicBool>);
    impl Inspector {
        fn new(dumpable: bool) -> Self {
            Self(Arc::new(AtomicBool::new(dumpable)))
        }
    }
    impl WorkerInspector for Inspector {
        fn worker_identity(&self, pid: u32) -> Result<WorkerIdentity, Error> {
            assert_eq!(pid, 42);
            Ok(WorkerIdentity {
                uid: 1000,
                gid: 1000,
                dumpable: self.0.load(Ordering::SeqCst),
            })
        }
    }
    fn broker_with(inspector: Inspector) -> Broker<Fs, Clone, Nonces, Inspector> {
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
            inspector,
        )
        .unwrap()
    }
    fn broker() -> Broker<Fs, Clone, Nonces, Inspector> {
        broker_with(Inspector::new(false))
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
            broker.create(connection, "other", nonce, "leaf"),
            Err(Error::InvalidInvocationBinding)
        ));
        let stale = nonce;
        let nonce = broker.create(connection, "one", nonce, "leaf").unwrap();
        // The old create nonce is now irreversibly stale and cannot replay a control.
        assert!(matches!(
            broker.lift(connection, "one", stale, 9),
            Err(Error::InvalidNonce)
        ));
        assert!(matches!(
            broker.lift(connection, "one", nonce, 8),
            Err(Error::FenceMismatch)
        ));
        assert!(matches!(
            Broker::<Fs, Clone, Nonces, Inspector>::new(
                Launcher::new(
                    Fs::ready(),
                    Clone,
                    crate::LauncherConfig::new(None, 0).unwrap()
                )
                .unwrap(),
                BrokerConfig::worker(42, b"x".to_vec()).unwrap(),
                Nonces(0),
                Inspector::new(true)
            ),
            Err(Error::InvalidWorker)
        ));
    }

    #[test]
    fn worker_is_reinspected_before_spawn() {
        let inspector = Inspector::new(false);
        let mut broker = broker_with(inspector.clone());
        let peer = FakePeer(UnixPeer {
            pid: 42,
            uid: 1000,
            gid: 1000,
        });
        let connection = broker.authenticate(&peer, b"private").unwrap();
        let nonce = broker
            .begin_invocation(
                connection,
                Invocation {
                    id: "one".into(),
                    fence: 9,
                },
            )
            .unwrap();
        inspector.0.store(true, Ordering::SeqCst);
        assert!(matches!(
            broker.create(connection, "one", nonce, "leaf"),
            Err(Error::InvalidWorker)
        ));
    }
}
