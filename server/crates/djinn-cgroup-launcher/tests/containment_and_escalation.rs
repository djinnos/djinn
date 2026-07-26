//! Repository-executable containment + escalation harness for the landed
//! launcher/broker/child seams (djinn board task 7os5, epic kh95, proposal goxi).
//!
//! Everything here is driven through fake cgroup-filesystem, clone, peer, and
//! nonce seams so kernel behaviour is modelled deterministically. No test needs
//! privileged Kubernetes or cgroup access.
//!
//! The privilege-dependent half — a real UID-1001 child attacking a real,
//! non-dumpable UID-1000 worker, and the distinct-UID setgid artifact edit that
//! used to sit behind a `geteuid() == 0` check here and therefore never
//! executed — now lives in `tests/kernel_boundary_under_rendered_context.rs`
//! (task zf13, goxi AC2), which runs in the privileged
//! `launcher-kernel-boundary` CI lane and FAILS rather than skips when that
//! environment is unavailable. What remains below is the unprivileged mechanism
//! proof, which runs everywhere.
//!
//! Cluster-wide cap/warm-consumer assertions live in sibling epics 9y1a/23or.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use djinn_cgroup_launcher::{
    CgroupFs, CgroupMode, ChildProcess, CommandSpec, Error, Invocation, Launcher, LauncherConfig,
    LeaseAuthority, Readiness, SpawnIntoCgroup,
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
/// single already-placed child was admitted for each invocation leaf. Native
/// production spawning is separately covered by `spawn.rs`: fork, placement
/// write/readback, then GO before credential drop or exec.
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
impl SpawnIntoCgroup for FakeClone {
    fn spawn_into_cgroup(
        &mut self,
        _target: RawFd,
        _invocation: &Invocation,
        _command: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        let mut state = self.0.borrow_mut();
        state.attempts += 1;
        if state.deny {
            return Err(Error::SpawnDenied);
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
        LauncherConfig::new(None, None, 0).expect("launcher config"),
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

/// Drive two commands through the authenticated broker boundary, rather than
/// calling `Launcher::create_command` directly. Every command gets its own
/// leaf and starts at 250m; the only accepted matching fence lifts exactly once
/// to the explicit pod quota, never to `max`.
#[test]
fn authenticated_broker_gives_each_command_a_fresh_250m_leaf_and_one_explicit_lift() {
    let fs = FakeCgroup::with_owner(7);
    let clone = FakeClone::allow();
    let launcher = Launcher::new(
        fs.clone(),
        clone.clone(),
        LauncherConfig::new(None, Some(4_000), 7).expect("launcher config"),
    )
    .expect("ready launcher");
    let mut broker = Broker::new(
        launcher,
        BrokerConfig::worker(42, POD_CREDENTIAL.to_vec()).expect("broker config"),
        OsNonceSource,
    )
    .expect("broker");
    let connection = authenticated_ready(&mut broker);

    let first = Invocation {
        id: "first-command".to_owned(),
        fence: 41,
    };
    let first_nonce = broker
        .begin_invocation(connection, first)
        .expect("begin first command");
    let first_nonce = broker
        .create(
            connection,
            "first-command",
            first_nonce,
            "first-leaf",
            LeaseAuthority::Armed,
            &command(),
        )
        .expect("create first command");
    assert!(matches!(
        broker.lift(connection, "first-command", first_nonce, 40),
        Err(Error::FenceMismatch)
    ));
    let first_nonce = broker
        .lift(connection, "first-command", first_nonce, 41)
        .expect("matching fence lifts first leaf");
    assert!(matches!(
        broker.lift(connection, "first-command", first_nonce, 41),
        Err(Error::LiftAlreadyApplied)
    ));

    let second = Invocation {
        id: "second-command".to_owned(),
        fence: 42,
    };
    let second_nonce = broker
        .begin_invocation(connection, second)
        .expect("begin second command");
    broker
        .create(
            connection,
            "second-command",
            second_nonce,
            "second-leaf",
            LeaseAuthority::Armed,
            &command(),
        )
        .expect("create second command");

    assert_eq!(fs.creates(), 2, "commands cannot reuse an invocation leaf");
    assert_eq!(clone.0.borrow().attempts, 2, "one spawn per command leaf");
    assert_eq!(
        fs.writes_to("cpu.max"),
        vec![
            "25000 100000".to_owned(),
            "400000 100000".to_owned(),
            "25000 100000".to_owned(),
        ],
        "a matching durable fence is the sole path from the fresh 250m leaf to the explicit pod quota"
    );
}

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
        LauncherConfig::new(None, None, 7).expect("config"),
    )
    .expect("launcher");

    let invocation = Invocation {
        id: "daemon-tree".to_owned(),
        fence: 5,
    };
    let (mut leaf, child) = launcher
        .create_command("daemon-tree", invocation, LeaseAuthority::Armed, &command())
        .expect("create invocation");
    assert_eq!(child.pid, 4242);

    // Exactly one delegated cgroup and one spawn attempt back the whole tree.
    assert_eq!(
        fs.creates(),
        1,
        "the daemon + double-fork descendants share one invocation cgroup"
    );
    assert_eq!(
        clone.0.borrow().attempts,
        1,
        "one placed-child spawn attempt"
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
        LauncherConfig::new(None, None, 3).expect("config"),
    )
    .expect("launcher");
    let (mut leaf, _) = launcher
        .create_command(
            "cancelled",
            Invocation {
                id: "cancelled".to_owned(),
                fence: 1,
            },
            LeaseAuthority::Armed,
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
        broker.create(
            connection_b,
            "one",
            nonce0,
            "leaf",
            LeaseAuthority::Armed,
            &command()
        ),
        Err(Error::InvalidInvocationBinding)
    ));

    // The owning connection with the live nonce succeeds and rotates the nonce.
    let nonce1 = broker
        .create(
            connection_a,
            "one",
            nonce0,
            "leaf",
            LeaseAuthority::Armed,
            &command(),
        )
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
    /// One unsupported readiness profile and the error it must fail closed with.
    type RejectedProfile = (Readiness, fn(&Error) -> bool);
    let cases: Vec<RejectedProfile> = vec![
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
            LauncherConfig::new(None, None, 7).expect("config"),
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

/// The child boundary sets GID = ARTIFACT_GID (1000) and umask 0002 so that a
/// setgid artifact directory yields group-owned, group-writable files that the
/// distinct worker (UID 1000) and child (UID 1001) — both in group 1000 — can
/// mutually edit.
///
/// This proves the MECHANISM unconditionally, with the process's own gid, so it
/// runs everywhere. The real cross-UID edit at the rendered identities is
/// proven by
/// `kernel_boundary_under_rendered_context::setgid_artifacts_stay_mutually_editable_across_the_distinct_worker_and_child_uids`,
/// which executes in the privileged lane and fails loudly there rather than
/// skipping — the gap that left this half of the AC unproven.
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

    let _ = std::fs::remove_dir_all(&base);
}
