//! Worker side of the cgroup-launcher handshake (task ab05).
//!
//! qut0 (#2533) renders a mandatory `cgroup-launcher` sidecar whose serve loop
//! (`djinn-cgroup-launcher` `src/main.rs::read_worker_handshake`) waits for a
//! worker handshake — a worker-private credential plus the worker PID written
//! into the shared IPC `Memory` emptyDir — before it binds its control socket
//! and serves. This module is the worker end of that handshake: it writes the
//! two files the launcher polls for, then dials the launcher's control socket
//! presenting the same credential.
//!
//! ## Byte-for-byte contract (do NOT invent a new format)
//!
//! The launcher reads exactly two paths inside the IPC mount: the credential
//! file at `DJINN_LAUNCHER_CREDENTIAL_PATH` (arbitrary non-empty bytes), and
//! `worker.pid` in the SAME directory (the worker PID as a trimmed decimal
//! string that parses to a non-zero `u32`).
//!
//! It then configures its broker with `(worker_pid, credential)` and binds the
//! socket at `DJINN_LAUNCHER_SOCKET`. The worker connects there with the same
//! credential; the broker authenticates the `SO_PEERCRED` `(pid, uid, gid)`
//! against `worker.pid`/`WORKER_UID`/`WORKER_GID` and the presented credential
//! against the file. See `WORKER_PID_FILE` below — it mirrors the launcher's own
//! constant so the two ends can never drift.
//!
//! ## Detection-gated fallback (never block pod startup)
//!
//! The IPC mount is the parent directory of the credential path. When it is
//! absent — an old rendering without the sidecar, or a local/non-pod run — the
//! handshake is skipped and the caller keeps the unleased in-process broker path
//! UNCHANGED. Any failure AFTER detection (write error, the socket never binds,
//! a credential/peer rejection, a readiness failure) is a typed, bounded outcome
//! that also falls back to the in-process path. The handshake never returns an
//! unbounded wait and never propagates an error that would fail pod startup.

use std::fs::File;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use djinn_cgroup_launcher::Error as LauncherError;
use djinn_cgroup_launcher::child::{WorkerDumpability, prepare_worker_readiness};
use djinn_cgroup_launcher::transport::UnixBrokerClient;

/// Worker→launcher PID handshake filename. MUST match `WORKER_PID_FILE` in
/// `djinn-cgroup-launcher` `src/main.rs`; the launcher joins it onto the
/// credential file's parent directory.
const WORKER_PID_FILE: &str = "worker.pid";

/// Size of the freshly generated worker-private credential.
const CREDENTIAL_BYTES: usize = 32;

/// Bounded wait for the launcher to bind its control socket after it has read
/// our handshake. The native sidecar is already running before the worker
/// starts, so it binds within a few poll ticks; the ceiling only guards a
/// sidecar that never serves, and it is a hard bound so pod startup can never
/// hang. 100ms × 300 = 30s.
const CONNECT_POLL: Duration = Duration::from_millis(100);
const CONNECT_ATTEMPTS: u32 = 300;

/// Typed, bounded outcome of the launcher handshake. Every variant other than
/// [`LauncherHandshake::Connected`] routes the worker to the unleased in-process
/// broker path unchanged.
pub enum LauncherHandshake {
    /// The sidecar is present and authenticated us: the broker client is live
    /// and readiness has been accepted.
    Connected(Box<UnixBrokerClient>),
    /// No IPC mount / sidecar (old rendering, or a local/non-pod run). Use the
    /// in-process path unchanged.
    AbsentMount,
    /// The mount was present but the handshake failed. Fail closed to the
    /// in-process path; the reason is carried for logging.
    FailedClosed(HandshakeError),
}

/// Bounded failure reasons for the post-detection handshake. All fold into
/// [`LauncherHandshake::FailedClosed`].
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("write worker launcher handshake: {0}")]
    Io(#[source] std::io::Error),
    #[error("launcher control socket never bound within the handshake window")]
    ConnectTimeout,
    #[error("connect launcher control socket: {0}")]
    Connect(#[source] std::io::Error),
    #[error("launcher rejected the worker handshake (credential or peer mismatch): {0}")]
    AuthRejected(#[source] LauncherError),
    #[error("prepare non-dumpable worker readiness: {0}")]
    Readiness(#[source] LauncherError),
    #[error("submit worker readiness: {0}")]
    Ready(#[source] LauncherError),
}

/// Establish the leased broker path, gated on the sidecar's presence.
///
/// `socket_path` is `DJINN_LAUNCHER_SOCKET` and `credential_path` is
/// `DJINN_LAUNCHER_CREDENTIAL_PATH` (both projected by qut0's render). The
/// `dumpability` seam is [`djinn_cgroup_launcher::child::NativeWorkerDumpability`]
/// in production; tests inject a double so the non-dumpable prctl is not applied
/// to the test process.
pub fn establish<D: WorkerDumpability>(
    socket_path: &Path,
    credential_path: &Path,
    dumpability: &mut D,
) -> LauncherHandshake {
    // Detection: the launcher IPC mount is the credential file's parent
    // directory (qut0's Memory emptyDir at LAUNCHER_IPC_DIR). Absent → the
    // sidecar was not rendered (or this is a local/non-pod run) → in-process.
    let Some(mount_dir) = credential_path.parent() else {
        return LauncherHandshake::AbsentMount;
    };
    if !mount_dir.is_dir() {
        return LauncherHandshake::AbsentMount;
    }
    match connect_leased(socket_path, credential_path, mount_dir, dumpability) {
        Ok(client) => LauncherHandshake::Connected(Box::new(client)),
        Err(error) => LauncherHandshake::FailedClosed(error),
    }
}

fn connect_leased<D: WorkerDumpability>(
    socket_path: &Path,
    credential_path: &Path,
    mount_dir: &Path,
    dumpability: &mut D,
) -> Result<UnixBrokerClient, HandshakeError> {
    // 1. Publish the worker handshake the launcher's serve loop waits for. The
    //    credential is published BEFORE `worker.pid` so the launcher — which
    //    only reads once BOTH files exist — never observes the pid without a
    //    complete credential beside it.
    let credential = ensure_credential(credential_path)?;
    write_worker_pid(mount_dir)?;

    // 2. Wait (bounded) for the launcher to bind its control socket after
    //    reading our handshake, then present the same credential.
    let mut client = connect_with_retry(socket_path, &credential)?;

    // 3. Prove non-dumpability on the authenticated connection. A failure here
    //    is security-critical, so it fails closed to the in-process path rather
    //    than serving a worker that could be ptraced.
    let readiness = prepare_worker_readiness(dumpability).map_err(HandshakeError::Readiness)?;
    client.ready(readiness).map_err(HandshakeError::Ready)?;
    Ok(client)
}

/// Return the worker-private credential, generating and atomically publishing a
/// fresh one only when the mount does not already hold it. Reusing a persisted
/// credential keeps a RESTARTED launcher — which re-reads the same file — able to
/// authenticate the worker's reconnect with the same secret.
fn ensure_credential(credential_path: &Path) -> Result<Vec<u8>, HandshakeError> {
    if let Ok(existing) = std::fs::read(credential_path)
        && !existing.is_empty()
    {
        return Ok(existing);
    }
    let mut credential = vec![0_u8; CREDENTIAL_BYTES];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut credential))
        .map_err(HandshakeError::Io)?;
    atomic_publish(credential_path, &credential)?;
    Ok(credential)
}

/// Publish the worker PID as a trimmed decimal string, mirroring the launcher's
/// `pid_text.trim().parse::<u32>()` read.
fn write_worker_pid(mount_dir: &Path) -> Result<(), HandshakeError> {
    let pid = std::process::id().to_string();
    atomic_publish(&mount_dir.join(WORKER_PID_FILE), pid.as_bytes())
}

/// Write `bytes` to `path` via a sibling temp file + rename, so a concurrent
/// launcher poll never observes a half-written file at `path`.
fn atomic_publish(path: &Path, bytes: &[u8]) -> Result<(), HandshakeError> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes).map_err(HandshakeError::Io)?;
    std::fs::rename(&temp, path).map_err(HandshakeError::Io)?;
    Ok(())
}

/// Dial the launcher's control socket, retrying only the pre-bind race.
///
/// A failed `UnixStream::connect` (the socket file not yet created, or no
/// listener yet) is the expected race while the launcher is still reading our
/// handshake, so it is retried within the bounded window. Once the stream
/// connects, the launcher is serving and the AUTH exchange is terminal either
/// way: a rejection there is a credential/peer mismatch, never a race, so it is
/// surfaced immediately rather than retried.
fn connect_with_retry(
    socket_path: &Path,
    credential: &[u8],
) -> Result<UnixBrokerClient, HandshakeError> {
    for _ in 0..CONNECT_ATTEMPTS {
        match UnixStream::connect(socket_path) {
            Ok(stream) => {
                return UnixBrokerClient::connect(stream, credential)
                    .map_err(HandshakeError::AuthRejected);
            }
            Err(error) if is_pre_bind_race(&error) => std::thread::sleep(CONNECT_POLL),
            Err(error) => return Err(HandshakeError::Connect(error)),
        }
    }
    Err(HandshakeError::ConnectTimeout)
}

/// A not-yet-bound control socket surfaces as `ENOENT` (the launcher has not
/// created the socket file yet) or `ECONNREFUSED` (a rebind window). Any other
/// connect error is a genuine, non-transient fault that fails closed at once.
fn is_pre_bind_race(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::fd::RawFd;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;

    use djinn_cgroup_launcher::broker::{Broker, BrokerConfig, OsNonceSource};
    use djinn_cgroup_launcher::child::WorkerDumpability;
    use djinn_cgroup_launcher::transport::UnixBrokerServer;
    use djinn_cgroup_launcher::{
        CgroupFs, CgroupMode, ChildProcess, CloneIntoCgroup, CommandSpec, Error, Invocation,
        Launcher, LauncherConfig, Readiness,
    };

    /// A dumpability double that reports the worker as already non-dumpable
    /// without touching the real process `prctl` state.
    struct FakeDumpability;
    impl WorkerDumpability for FakeDumpability {
        fn set_non_dumpable(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn get_dumpable(&mut self) -> Result<i32, Error> {
            Ok(0)
        }
    }

    /// Minimal cgroup filesystem double: readiness passes so `Launcher::new`
    /// succeeds; no leaf/clone method is reached because the handshake tests
    /// stop at AUTH + READY.
    struct FakeFs {
        owner_uid: u32,
    }
    impl CgroupFs for FakeFs {
        fn readiness(&self) -> Result<Readiness, Error> {
            Ok(Readiness {
                mode: CgroupMode::V2,
                root_writable: true,
                owner_uid: self.owner_uid,
                delegated_controllers: BTreeSet::from(["cpu".to_string()]),
            })
        }
        fn create_direct_child(&mut self, _: &str) -> Result<RawFd, Error> {
            Err(Error::CloneDenied)
        }
        fn write_leaf(&mut self, _: RawFd, _: &str, _: &str) -> Result<(), Error> {
            Err(Error::CloneDenied)
        }
        fn read_leaf(&mut self, _: RawFd, _: &str) -> Result<String, Error> {
            Err(Error::CloneDenied)
        }
        fn remove_leaf(&mut self, _: RawFd, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }
    struct NoClone;
    impl CloneIntoCgroup for NoClone {
        fn clone_into_cgroup(
            &mut self,
            _: RawFd,
            _: &Invocation,
            _: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            Err(Error::CloneDenied)
        }
    }

    /// A faithful fake sidecar serve loop: poll for the worker handshake exactly
    /// as the real launcher does, then bind the control socket and serve.
    ///
    /// `credential_override` forces the broker to expect a DIFFERENT credential
    /// than the worker wrote, exercising the fail-closed credential-mismatch
    /// path. `serve_forever` loops `serve_once` so a dropped client is followed
    /// by a fresh accept (used by the restart test); otherwise a single
    /// connection is served.
    fn spawn_fake_launcher(
        ipc_dir: PathBuf,
        socket_path: PathBuf,
        credential_override: Option<Vec<u8>>,
        serve_forever: bool,
        bound_tx: mpsc::Sender<()>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let credential_path = ipc_dir.join("credential");
            let pid_path = ipc_dir.join(WORKER_PID_FILE);
            // Mirror read_worker_handshake: wait for both files, then read.
            let (worker_pid, file_credential) = loop {
                if credential_path.exists() && pid_path.exists() {
                    let bytes = std::fs::read(&credential_path).expect("read worker credential");
                    let pid_text =
                        std::fs::read_to_string(&pid_path).expect("read worker.pid handshake");
                    let pid: u32 = pid_text.trim().parse().expect("worker.pid is decimal");
                    assert!(
                        !bytes.is_empty() && pid != 0,
                        "handshake must be non-trivial"
                    );
                    break (pid, bytes);
                }
                thread::sleep(Duration::from_millis(10));
            };
            let pod_credential = credential_override.unwrap_or(file_credential);
            // The connecting peer is the test process itself (establish runs
            // in-process), so authenticate against this process's real uid/gid
            // and the PID the worker published.
            let owner_uid = unsafe { libc::geteuid() };
            let config = BrokerConfig {
                worker_pid,
                worker_uid: owner_uid,
                worker_gid: unsafe { libc::getegid() },
                pod_credential,
            };
            let launcher = Launcher::new(
                FakeFs { owner_uid },
                NoClone,
                LauncherConfig::new(None, owner_uid).expect("launcher config"),
            )
            .expect("launcher readiness");
            let broker = Broker::new(launcher, config, OsNonceSource).expect("broker");
            let mut server =
                UnixBrokerServer::bind(&socket_path, broker).expect("bind fake launcher socket");
            let _ = bound_tx.send(());
            if serve_forever {
                // One connection, then keep accepting so a reconnect after the
                // client drops is served too.
                let _ = server.serve_once();
                let _ = server.serve_once();
            } else {
                let _ = server.serve_once();
            }
        })
    }

    fn ipc_paths(dir: &Path) -> (PathBuf, PathBuf) {
        (dir.join("broker.sock"), dir.join("credential"))
    }

    #[test]
    fn successful_handshake_authenticates_and_submits_readiness() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket_path, credential_path) = ipc_paths(dir.path());
        let (tx, _rx) = mpsc::channel();
        let launcher = spawn_fake_launcher(
            dir.path().to_path_buf(),
            socket_path.clone(),
            None,
            false,
            tx,
        );

        let outcome = establish(&socket_path, &credential_path, &mut FakeDumpability);
        match outcome {
            LauncherHandshake::Connected(client) => drop(client),
            other => panic!("expected Connected, got {}", describe(&other)),
        }
        launcher.join().expect("fake launcher thread");

        // The worker published both handshake files the launcher reads.
        assert!(credential_path.exists(), "credential must be published");
        assert!(
            dir.path().join(WORKER_PID_FILE).exists(),
            "worker.pid must be published"
        );
    }

    #[test]
    fn absent_mount_falls_back_without_touching_the_socket() {
        // The credential path's parent does not exist → detection short-circuits
        // to the in-process path with no file writes and no socket dial.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-mount");
        let socket_path = missing.join("broker.sock");
        let credential_path = missing.join("credential");

        let outcome = establish(&socket_path, &credential_path, &mut FakeDumpability);
        assert!(
            matches!(outcome, LauncherHandshake::AbsentMount),
            "absent mount must fall back to the in-process path"
        );
        assert!(
            !credential_path.exists(),
            "no credential is written on fallback"
        );
    }

    #[test]
    fn credential_mismatch_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket_path, credential_path) = ipc_paths(dir.path());
        let (tx, _rx) = mpsc::channel();
        // The launcher expects a different credential than the worker will write.
        let launcher = spawn_fake_launcher(
            dir.path().to_path_buf(),
            socket_path.clone(),
            Some(b"a-different-secret".to_vec()),
            false,
            tx,
        );

        let outcome = establish(&socket_path, &credential_path, &mut FakeDumpability);
        assert!(
            matches!(
                outcome,
                LauncherHandshake::FailedClosed(HandshakeError::AuthRejected(_))
            ),
            "a credential mismatch must fail closed to the in-process path"
        );
        launcher.join().expect("fake launcher thread");
    }

    #[test]
    fn sidecar_restart_reuses_the_persisted_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket_path, credential_path) = ipc_paths(dir.path());

        // First sidecar instance: serve the initial connection.
        let (tx1, _rx1) = mpsc::channel();
        let first = spawn_fake_launcher(
            dir.path().to_path_buf(),
            socket_path.clone(),
            None,
            false,
            tx1,
        );
        match establish(&socket_path, &credential_path, &mut FakeDumpability) {
            LauncherHandshake::Connected(client) => drop(client),
            other => panic!("first connect expected Connected, got {}", describe(&other)),
        }
        first.join().expect("first fake launcher thread");
        let published =
            std::fs::read(&credential_path).expect("credential persists across restart");

        // Second sidecar instance reads the SAME persisted handshake files.
        let (tx2, rx2) = mpsc::channel();
        let second = spawn_fake_launcher(
            dir.path().to_path_buf(),
            socket_path.clone(),
            None,
            false,
            tx2,
        );
        rx2.recv().expect("second sidecar binds its socket");

        // The reconnect reuses the persisted credential, so the restarted
        // launcher authenticates it.
        match establish(&socket_path, &credential_path, &mut FakeDumpability) {
            LauncherHandshake::Connected(client) => drop(client),
            other => panic!("reconnect expected Connected, got {}", describe(&other)),
        }
        second.join().expect("second fake launcher thread");
        assert_eq!(
            std::fs::read(&credential_path).expect("credential still present"),
            published,
            "the worker must reuse the persisted per-pod credential across a restart"
        );
    }

    fn describe(outcome: &LauncherHandshake) -> String {
        match outcome {
            LauncherHandshake::Connected(_) => "Connected".to_string(),
            LauncherHandshake::AbsentMount => "AbsentMount".to_string(),
            LauncherHandshake::FailedClosed(error) => format!("FailedClosed({error})"),
        }
    }
}
