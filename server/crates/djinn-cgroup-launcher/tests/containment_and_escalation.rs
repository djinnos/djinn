//! Repository-executable containment + escalation harness for the landed
//! launcher/broker/child seams (djinn board task 7os5, epic kh95, proposal goxi).
//!
//! Everything here is driven through fake cgroup-filesystem, clone, peer, and
//! nonce seams so kernel behaviour is modelled deterministically. No test needs
//! privileged Kubernetes or cgroup access. The one privilege-dependent
//! assertion (distinct-UID setgid artifact editing) always runs its unprivileged
//! mechanism proof and additionally runs the real distinct-UID edit — asserting,
//! never silently passing — when the environment can switch identity.
//!
//! Rendered security-context and cluster-wide cap/warm-consumer assertions live
//! in sibling epics 9y1a/23or, not here.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use djinn_cgroup_launcher::{
    CgroupFs, CgroupMode, ChildProcess, CloneIntoCgroup, CommandSpec, Error, Invocation, Launcher,
    LauncherConfig, Readiness,
    broker::{
        Broker, BrokerConfig, OsNonceSource, PeerCredentials, UnixPeer, WORKER_GID, WORKER_UID,
    },
    child::{
        ARTIFACT_GID, CHILD_UID, ChildDescriptor, ChildMounts, ChildSyscalls, DescriptorKind,
        WorkerDumpability, WorkerReadinessAssertion, prepare_child, prepare_worker_readiness,
    },
};

// ─────────────────────────── fake kernel seams ───────────────────────────

/// Injectable cgroup filesystem. `cpu.stat` and `cgroup.events` are mutable
/// between calls so tests can model consumption growth and descendant liveness.
#[derive(Clone)]
struct FakeCgroup(Rc<RefCell<CgState>>);

struct CgState {
    readiness: Readiness,
    cpu_stat: String,
    events: String,
    writes: Vec<(String, String)>,
    creates: usize,
    removes: usize,
    next_fd: RawFd,
}

impl FakeCgroup {
    fn with_owner(owner_uid: u32) -> Self {
        Self(Rc::new(RefCell::new(CgState {
            readiness: Readiness {
                mode: CgroupMode::V2,
                root_writable: true,
                owner_uid,
                delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
            },
            // A fresh leaf reports no populated descendants until a child exists.
            cpu_stat: "usage_usec 0".to_owned(),
            events: "populated 1\n".to_owned(),
            writes: Vec::new(),
            creates: 0,
            removes: 0,
            next_fd: 100,
        })))
    }

    fn with_readiness(readiness: Readiness) -> Self {
        let fs = Self::with_owner(readiness.owner_uid);
        fs.0.borrow_mut().readiness = readiness;
        fs
    }

    fn set_events(&self, value: &str) {
        self.0.borrow_mut().events = value.to_owned();
    }
    fn creates(&self) -> usize {
        self.0.borrow().creates
    }
    fn removes(&self) -> usize {
        self.0.borrow().removes
    }
    fn writes_to(&self, file: &str) -> Vec<String> {
        self.0
            .borrow()
            .writes
            .iter()
            .filter(|(f, _)| f == file)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

impl CgroupFs for FakeCgroup {
    fn readiness(&self) -> Result<Readiness, Error> {
        Ok(self.0.borrow().readiness.clone())
    }
    fn create_direct_child(&mut self, _name: &str) -> Result<RawFd, Error> {
        let mut state = self.0.borrow_mut();
        state.creates += 1;
        state.next_fd += 1;
        Ok(state.next_fd)
    }
    fn write_leaf(&mut self, _fd: RawFd, file: &str, value: &str) -> Result<(), Error> {
        self.0
            .borrow_mut()
            .writes
            .push((file.to_owned(), value.to_owned()));
        Ok(())
    }
    fn read_leaf(&mut self, _fd: RawFd, file: &str) -> Result<String, Error> {
        let state = self.0.borrow();
        Ok(match file {
            "cpu.stat" => state.cpu_stat.clone(),
            "cgroup.events" => state.events.clone(),
            _ => String::new(),
        })
    }
    fn remove_leaf(&mut self, _fd: RawFd, _name: &str) -> Result<(), Error> {
        self.0.borrow_mut().removes += 1;
        Ok(())
    }
}

/// The only child-spawn seam. It never runs a real process; it records that a
/// single clone3-into-cgroup attempt occurred for the whole invocation tree.
#[derive(Clone)]
struct FakeClone(Rc<RefCell<CloneState>>);
struct CloneState {
    attempts: usize,
    deny: bool,
}
impl FakeClone {
    fn allow() -> Self {
        Self(Rc::new(RefCell::new(CloneState {
            attempts: 0,
            deny: false,
        })))
    }
}
impl CloneIntoCgroup for FakeClone {
    fn clone_into_cgroup(
        &mut self,
        _target: RawFd,
        _invocation: &Invocation,
        _command: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        let mut state = self.0.borrow_mut();
        state.attempts += 1;
        if state.deny {
            return Err(Error::CloneDenied);
        }
        Ok(ChildProcess {
            pid: 4242,
            stdout: -1,
            stderr: -1,
        })
    }
}

#[derive(Clone, Copy)]
struct FakePeer(UnixPeer);
impl PeerCredentials for FakePeer {
    fn peer_credentials(&self) -> Result<UnixPeer, Error> {
        Ok(self.0)
    }
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

fn command() -> CommandSpec {
    CommandSpec {
        program: "/bin/true".to_owned(),
        argv: vec![],
        cwd: "/workspace".to_owned(),
        environment: vec![],
    }
}

fn worker_readiness() -> WorkerReadinessAssertion {
    prepare_worker_readiness(&mut ReadyDumpability).expect("worker readiness")
}

const POD_CREDENTIAL: &[u8] = b"per-pod-private-credential";

fn broker() -> Broker<FakeCgroup, FakeClone, OsNonceSource> {
    let launcher = Launcher::new(
        FakeCgroup::with_owner(0),
        FakeClone::allow(),
        LauncherConfig::new(None, 0).expect("launcher config"),
    )
    .expect("launcher");
    Broker::new(
        launcher,
        BrokerConfig::worker(42, POD_CREDENTIAL.to_vec()).expect("broker config"),
        OsNonceSource,
    )
    .expect("broker")
}

fn authenticated_ready(
    broker: &mut Broker<FakeCgroup, FakeClone, OsNonceSource>,
) -> djinn_cgroup_launcher::broker::ConnectionId {
    let peer = FakePeer(UnixPeer {
        pid: 42,
        uid: WORKER_UID,
        gid: WORKER_GID,
    });
    let connection = broker
        .authenticate(&peer, POD_CREDENTIAL)
        .expect("authenticate worker");
    broker
        .accept_worker_readiness(connection, worker_readiness())
        .expect("accept worker readiness");
    connection
}

// ══════════════════════════════ AC 2 ══════════════════════════════
// Daemon and double-fork descendants remain in the invocation cgroup;
// cancellation/timeout kills all descendants; release/cleanup is impossible
// until fake `cgroup.events` reports `populated 0`.

/// A daemon or double-forked grandchild reparents to init and escapes any
/// pid-tree reaper, but it is born inside the single invocation cgroup and is
/// still counted by `cgroup.events`. Release therefore stays impossible until
/// the whole subtree is empty, and a single `cgroup.kill` collapses the tree.
#[test]
fn ac2_daemon_and_double_fork_stay_in_one_cgroup_and_release_gates_on_populated_zero() {
    let fs = FakeCgroup::with_owner(7);
    let clone = FakeClone::allow();
    let mut launcher = Launcher::new(
        fs.clone(),
        clone.clone(),
        LauncherConfig::new(None, 7).expect("config"),
    )
    .expect("launcher");

    let invocation = Invocation {
        id: "daemon-tree".to_owned(),
        fence: 5,
    };
    let (mut leaf, child) = launcher
        .create_command("daemon-tree", invocation, &command())
        .expect("create invocation");
    assert_eq!(child.pid, 4242);

    // Exactly one delegated cgroup and one clone3 attempt back the whole tree.
    assert_eq!(
        fs.creates(),
        1,
        "the daemon + double-fork descendants share one invocation cgroup"
    );
    assert_eq!(
        clone.0.borrow().attempts,
        1,
        "one clone3-into-cgroup attempt"
    );

    // The unleased quota is applied to the leaf before any lift.
    assert_eq!(fs.writes_to("cpu.max"), vec!["25000 100000".to_owned()]);

    // The direct child has exited but a reparented daemon is still in the
    // cgroup: cgroup.events reports it populated, so release must fail closed.
    fs.set_events("populated 1\n");
    assert!(matches!(
        launcher.wait_empty(&leaf),
        Err(Error::StillPopulated)
    ));
    assert!(matches!(launcher.remove(&leaf), Err(Error::StillPopulated)));
    assert_eq!(
        fs.removes(),
        0,
        "no cgroup is unlinked while still populated"
    );

    // One subtree kill terminates every descendant regardless of reparenting;
    // it is a single cgroup.kill write, not a per-pid signal fan-out.
    launcher.kill(&mut leaf).expect("cgroup kill");
    assert_eq!(fs.writes_to("cgroup.kill"), vec!["1".to_owned()]);

    // Terminal intent is latched before the kill, so a replayed grant can never
    // lift the (cancellation/timeout) killed tree afterwards.
    assert!(matches!(
        launcher.fenced_lift(&mut leaf, 5),
        Err(Error::TerminalIntent)
    ));

    // Only once the kernel reports the subtree drained can cleanup unlink it.
    fs.set_events("populated 0\n");
    launcher.remove(&leaf).expect("remove drained cgroup");
    assert_eq!(
        fs.removes(),
        1,
        "the invocation cgroup is unlinked exactly once"
    );
}

/// Cancellation and timeout are the same launcher primitive: mark terminal, then
/// one cgroup.kill. Neither path can unlink a cgroup that is still populated.
#[test]
fn ac2_cleanup_after_kill_still_requires_empty_before_unlink() {
    let fs = FakeCgroup::with_owner(3);
    let mut launcher = Launcher::new(
        fs.clone(),
        FakeClone::allow(),
        LauncherConfig::new(None, 3).expect("config"),
    )
    .expect("launcher");
    let (mut leaf, _) = launcher
        .create_command(
            "cancelled",
            Invocation {
                id: "cancelled".to_owned(),
                fence: 1,
            },
            &command(),
        )
        .expect("create");

    launcher.kill(&mut leaf).expect("kill");
    // A descendant survived the signal delivery window (uninterruptible IO):
    fs.set_events("populated 1\n");
    assert!(matches!(launcher.remove(&leaf), Err(Error::StillPopulated)));
    fs.set_events("populated 0\n");
    launcher.remove(&leaf).expect("remove after drain");
    assert_eq!(fs.writes_to("cgroup.kill"), vec!["1".to_owned()]);
}

// ══════════════════════════════ AC 3 ══════════════════════════════
// Reject wrong peer PID/UID, child/sibling connections, forged own-or-sibling
// controls, nonce replay, credential/control-descriptor exposure, and
// incompatible readiness; prove distinct worker/child UIDs can mutually edit
// setgid GID-1000 files under umask 0002.

#[test]
fn ac3_peer_authentication_rejects_wrong_pid_uid_child_and_sibling() {
    let mut broker = broker();
    let good = FakePeer(UnixPeer {
        pid: 42,
        uid: WORKER_UID,
        gid: WORKER_GID,
    });
    // A forked child of the worker has a different PID (and normally a dropped UID).
    let worker_child = FakePeer(UnixPeer {
        pid: 43,
        uid: CHILD_UID,
        gid: WORKER_GID,
    });
    // A sibling pod process shares the worker UID but has a different PID.
    let sibling = FakePeer(UnixPeer {
        pid: 44,
        uid: WORKER_UID,
        gid: WORKER_GID,
    });
    let wrong_uid = FakePeer(UnixPeer {
        pid: 42,
        uid: WORKER_UID + 7,
        gid: WORKER_GID,
    });

    assert!(matches!(
        broker.authenticate(&worker_child, POD_CREDENTIAL),
        Err(Error::UnauthenticatedPeer)
    ));
    assert!(matches!(
        broker.authenticate(&sibling, POD_CREDENTIAL),
        Err(Error::UnauthenticatedPeer)
    ));
    assert!(matches!(
        broker.authenticate(&wrong_uid, POD_CREDENTIAL),
        Err(Error::UnauthenticatedPeer)
    ));
    // Correct peer but a forged worker-private credential is rejected too.
    assert!(matches!(
        broker.authenticate(&good, b"forged-credential"),
        Err(Error::InvalidCredential)
    ));
    // Only the exact peer + credential authenticates.
    assert!(broker.authenticate(&good, POD_CREDENTIAL).is_ok());
}

#[test]
fn ac3_forged_own_or_sibling_controls_and_nonce_replay_are_rejected() {
    let mut broker = broker();
    let connection_a = authenticated_ready(&mut broker);
    let nonce0 = broker
        .begin_invocation(
            connection_a,
            Invocation {
                id: "one".to_owned(),
                fence: 9,
            },
        )
        .expect("begin invocation");

    // A second authenticated worker connection (a sibling session) may not
    // drive an invocation bound to another connection.
    let connection_b = authenticated_ready(&mut broker);
    assert!(matches!(
        broker.create(connection_b, "one", nonce0, "leaf", &command()),
        Err(Error::InvalidInvocationBinding)
    ));

    // The owning connection with the live nonce succeeds and rotates the nonce.
    let nonce1 = broker
        .create(connection_a, "one", nonce0, "leaf", &command())
        .expect("owning create");

    // Replaying the now-consumed nonce is a forged own-control: rejected.
    assert!(matches!(
        broker.lift(connection_a, "one", nonce0, 9),
        Err(Error::InvalidNonce)
    ));
    // A control naming an invocation this connection never began is rejected.
    assert!(matches!(
        broker.lift(connection_a, "sibling-invocation", nonce1, 9),
        Err(Error::InvalidInvocationBinding)
    ));
    // The live rotated nonce still authorises exactly one control.
    broker
        .lift(connection_a, "one", nonce1, 9)
        .expect("live rotated nonce lifts");
}

/// Every credential/control descriptor is closed before the irreversible UID
/// drop, and any visible broker/control mount fails the child closed.
#[test]
fn ac3_credential_and_control_descriptors_are_closed_before_credential_drop() {
    #[derive(Default)]
    struct Recording {
        log: Vec<String>,
    }
    impl ChildSyscalls for Recording {
        fn close(&mut self, fd: RawFd) -> Result<(), Error> {
            self.log.push(format!("close:{fd}"));
            Ok(())
        }
        fn set_groups_empty(&mut self) -> Result<(), Error> {
            self.log.push("groups".to_owned());
            Ok(())
        }
        fn clear_capabilities(&mut self) -> Result<(), Error> {
            self.log.push("caps".to_owned());
            Ok(())
        }
        fn set_gid(&mut self, gid: u32) -> Result<(), Error> {
            self.log.push(format!("gid:{gid}"));
            Ok(())
        }
        fn set_uid(&mut self, uid: u32) -> Result<(), Error> {
            self.log.push(format!("uid:{uid}"));
            Ok(())
        }
        fn set_umask(&mut self, mask: u32) -> Result<(), Error> {
            self.log.push(format!("umask:{mask:o}"));
            Ok(())
        }
        fn set_no_new_privs(&mut self) -> Result<(), Error> {
            self.log.push("nnp".to_owned());
            Ok(())
        }
        fn install_restricted_seccomp(&mut self) -> Result<(), Error> {
            self.log.push("seccomp".to_owned());
            Ok(())
        }
    }

    let mut child = Recording::default();
    prepare_child(
        &mut child,
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
            // An ordinary descriptor is never force-closed by preparation.
            ChildDescriptor {
                fd: 13,
                kind: DescriptorKind::Ordinary,
            },
        ],
        &ChildMounts::isolated(),
    )
    .expect("child preparation");

    // Every protected descriptor is closed first, and the ordinary fd is not.
    assert_eq!(
        &child.log[..4],
        ["close:9", "close:10", "close:11", "close:12"]
    );
    assert!(!child.log.iter().any(|c| c == "close:13"));

    // The irreversible credential drops happen strictly after the closes, and
    // target exactly the child artifact identity under umask 0002.
    let gid = child.log.iter().position(|c| c == "gid:1000").expect("gid");
    let uid = child.log.iter().position(|c| c == "uid:1001").expect("uid");
    let umask = child
        .log
        .iter()
        .position(|c| c == "umask:2")
        .expect("umask 0002");
    assert!(
        gid > 3 && uid > gid && umask > uid,
        "drop order: {:?}",
        child.log
    );
    assert!(child.log.iter().any(|c| c == "seccomp"));

    // A child that would still see any privileged mount fails closed.
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
        assert!(matches!(
            prepare_child(&mut Recording::default(), &[], &mounts),
            Err(Error::ChildIsolationViolation)
        ));
    }
}

/// Unsupported runtime readiness fails closed: non-v2/read-only/wrong-owner/
/// over-broad delegation is refused at launcher construction, and an
/// unauthenticated-for-readiness connection cannot begin an invocation.
#[test]
fn ac3_incompatible_readiness_profiles_and_missing_worker_readiness_fail_closed() {
    let base = || Readiness {
        mode: CgroupMode::V2,
        root_writable: true,
        owner_uid: 7,
        delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
    };
    let cases: Vec<(Readiness, fn(&Error) -> bool)> = vec![
        (
            Readiness {
                mode: CgroupMode::V1,
                ..base()
            },
            |e| matches!(e, Error::NotCgroupV2),
        ),
        (
            Readiness {
                mode: CgroupMode::Hybrid,
                ..base()
            },
            |e| matches!(e, Error::NotCgroupV2),
        ),
        (
            Readiness {
                root_writable: false,
                ..base()
            },
            |e| matches!(e, Error::ReadOnlyDelegation),
        ),
        (
            Readiness {
                owner_uid: 8,
                ..base()
            },
            |e| matches!(e, Error::IncompatibleOwnership { .. }),
        ),
        (
            Readiness {
                delegated_controllers: BTreeSet::new(),
                ..base()
            },
            |e| matches!(e, Error::OverbroadOrMissingCpuDelegation),
        ),
        (
            Readiness {
                // A broader-than-cpu delegation would give descendants an
                // escape surface (memory/cpuset/pids controllers).
                delegated_controllers: BTreeSet::from(["cpu".to_owned(), "memory".to_owned()]),
                ..base()
            },
            |e| matches!(e, Error::OverbroadOrMissingCpuDelegation),
        ),
    ];
    for (readiness, expected) in cases {
        let result = Launcher::new(
            FakeCgroup::with_readiness(readiness),
            FakeClone::allow(),
            LauncherConfig::new(None, 7).expect("config"),
        );
        match result {
            Err(error) => assert!(expected(&error), "unexpected error {error:?}"),
            Ok(_) => panic!("incompatible readiness must fail closed"),
        }
    }

    // A connection that authenticated but never asserted worker readiness
    // (unsupported runtime readiness) cannot begin an invocation.
    let mut broker = broker();
    let peer = FakePeer(UnixPeer {
        pid: 42,
        uid: WORKER_UID,
        gid: WORKER_GID,
    });
    let connection = broker
        .authenticate(&peer, POD_CREDENTIAL)
        .expect("authenticate");
    assert!(matches!(
        broker.begin_invocation(
            connection,
            Invocation {
                id: "no-readiness".to_owned(),
                fence: 1,
            },
        ),
        Err(Error::InvalidWorker)
    ));
}

// ─────────── AC 3: setgid GID-1000 + umask 0002 artifact editing ───────────

fn target_tmpdir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("set mode");
}

/// Whether this process can switch to arbitrary worker/child identities. Only
/// then is the real distinct-UID mutual edit exercised; the mechanism proof
/// below always runs.
fn can_switch_identity() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// The child boundary sets GID = ARTIFACT_GID (1000) and umask 0002 so that a
/// setgid artifact directory yields group-owned, group-writable files that the
/// distinct worker (UID 1000) and child (UID 1001) — both in group 1000 — can
/// mutually edit. This proves the mechanism unconditionally and, where the
/// environment permits identity switching, the real cross-UID edit as well.
#[test]
fn ac3_setgid_gid1000_umask0002_enables_distinct_uid_mutual_edit() {
    // The identities that make mutual editing possible: two distinct UIDs that
    // share the artifact group.
    assert_eq!(ARTIFACT_GID, 1000);
    assert_eq!(WORKER_UID, 1000);
    assert_eq!(WORKER_GID, 1000);
    assert_eq!(CHILD_UID, 1001);
    assert_ne!(
        WORKER_UID, CHILD_UID,
        "worker and child must be distinct UIDs"
    );

    let base = target_tmpdir().join(format!("djinn-7os5-artifact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create base");

    // Unprivileged mechanism proof: a setgid directory propagates its group to
    // new files, and umask 0002 leaves those files group-writable — the exact
    // combination that lets any same-group UID edit another's artifacts. Uses
    // the process's own gid so it runs everywhere and fails loud if broken.
    {
        use std::os::unix::fs::MetadataExt;
        let dir = base.join("mechanism");
        std::fs::create_dir(&dir).expect("mechanism dir");
        set_mode(&dir, 0o2775); // rwxrwsr-x: setgid set.
        let own_gid = unsafe { libc::getgid() };
        let previous = unsafe { libc::umask(0o002) };
        let artifact = dir.join("artifact.txt");
        std::fs::write(&artifact, b"seed").expect("write artifact");
        unsafe {
            libc::umask(previous);
        }
        let metadata = std::fs::metadata(&artifact).expect("artifact metadata");
        assert_eq!(
            metadata.gid(),
            own_gid,
            "a setgid directory must propagate its group to new files"
        );
        assert_ne!(
            metadata.mode() & 0o020,
            0,
            "umask 0002 must leave new files group-writable"
        );
    }

    // Privileged proof: two real processes at the distinct worker/child UIDs,
    // both in artifact group 1000, mutually edit each other's setgid artifacts.
    if can_switch_identity() {
        let dir = base.join("cross-uid");
        std::fs::create_dir(&dir).expect("cross-uid dir");
        set_mode(&dir, 0o2775);
        chown(&dir, WORKER_UID, ARTIFACT_GID);

        // Worker (uid 1000) creates an artifact; child (uid 1001) edits it.
        let worker_artifact = dir.join("worker-owned.txt");
        assert_eq!(create_as(&worker_artifact, WORKER_UID, ARTIFACT_GID), 0);
        assert_eq!(append_as(&worker_artifact, CHILD_UID, ARTIFACT_GID), 0);

        // And the reverse: child creates, worker edits.
        let child_artifact = dir.join("child-owned.txt");
        assert_eq!(create_as(&child_artifact, CHILD_UID, ARTIFACT_GID), 0);
        assert_eq!(append_as(&child_artifact, WORKER_UID, ARTIFACT_GID), 0);
    }

    let _ = std::fs::remove_dir_all(&base);
}

fn chown(path: &Path, uid: u32, gid: u32) {
    let c = CString::new(path.as_os_str().to_str().expect("utf8 path")).expect("cstring");
    let rc = unsafe { libc::chown(c.as_ptr(), uid, gid) };
    assert_eq!(rc, 0, "chown must succeed when privileged");
}

/// Fork a child that becomes (uid, gid) under umask 0002 and O_CREAT-writes the
/// file. Returns the child's exit code (0 on success). Only async-signal-safe
/// libc calls run post-fork.
fn create_as(path: &Path, uid: u32, gid: u32) -> i32 {
    run_as(
        path,
        uid,
        gid,
        libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
    )
}

/// Fork a child that becomes (uid, gid) and appends to an existing file,
/// proving a distinct same-group UID can edit another identity's artifact.
fn append_as(path: &Path, uid: u32, gid: u32) -> i32 {
    run_as(path, uid, gid, libc::O_WRONLY | libc::O_APPEND)
}

fn run_as(path: &Path, uid: u32, gid: u32, open_flags: i32) -> i32 {
    let c_path = CString::new(path.as_os_str().to_str().expect("utf8 path")).expect("cstring");
    let payload = b"edit\n";
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork must succeed");
    if pid == 0 {
        // Child: only async-signal-safe syscalls from here on.
        unsafe {
            libc::umask(0o002);
            let group = [gid];
            if libc::setgroups(1, group.as_ptr()) != 0 {
                libc::_exit(101);
            }
            if libc::setresgid(gid, gid, gid) != 0 {
                libc::_exit(102);
            }
            if libc::setresuid(uid, uid, uid) != 0 {
                libc::_exit(103);
            }
            let fd = libc::open(c_path.as_ptr(), open_flags, 0o666);
            if fd < 0 {
                libc::_exit(104);
            }
            let written = libc::write(fd, payload.as_ptr().cast(), payload.len());
            libc::close(fd);
            if written != payload.len() as isize {
                libc::_exit(105);
            }
            libc::_exit(0);
        }
    }
    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    assert_eq!(waited, pid, "waitpid must reap the child");
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    }
}
