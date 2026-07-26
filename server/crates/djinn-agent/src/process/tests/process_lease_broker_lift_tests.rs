//! The production broker composition, driven against a REAL broker.
//!
//! # The coverage gap this closes (goxi launcher blocker 14)
//!
//! Every other test in this tree drives a `CgroupLauncherClient` **double**. A
//! double's `fenced_lift` returns `Ok(())`, so the only thing those tests can
//! observe is that the runner *called* it. #2627 shipped on exactly that
//! assertion — plus "the birth authority was `Armed`" — and both were true in
//! production while the privileged broker refused every single lift:
//!
//! ```text
//! error=failed to run shell command: lease invocation failed:
//!       Launcher(Custom { kind: Other, error: InvalidControl })
//! ```
//!
//! The cause was a fence the composition could never satisfy.
//! [`UnixBrokerLauncher`] sent a launcher-wide constant `0` in the `BEGIN`
//! control that BORE the leaf, and the coordinator's durable
//! `build_lease_fencing_token_seq` value in the `LIFT` control. That sequence
//! starts at 1, so the two were never equal and `Launcher::fenced_lift` returned
//! `FenceMismatch` — 100% of the time, deterministically, for every armed
//! invocation.
//!
//! So these tests take the real [`UnixBrokerLauncher`] and the real
//! [`UnixBrokerClient`], connect them to a real [`UnixBrokerServer`] over a real
//! Unix socket, and assert on the `cpu.max` values the broker's launcher
//! actually wrote through its filesystem seam. Nothing here can pass while the
//! broker refuses the control.
//!
//! The kernel half — that the written line is one cgroup2 really honours, and
//! that the leaf's throughput multiplies — is
//! `djinn-cgroup-launcher/tests/brokered_lease_lift_boundary.rs`, run by the
//! `launcher-kernel-boundary` CI lane.

use super::*;
use djinn_cgroup_launcher::broker::{Broker, BrokerConfig, OsNonceSource};
use djinn_cgroup_launcher::child::{WorkerDumpability, prepare_worker_readiness};
use djinn_cgroup_launcher::transport::{UnixBrokerClient, UnixBrokerServer};
use djinn_cgroup_launcher::{
    CgroupFs, CgroupMode, ChildProcess, ControlRejection, Error as LauncherError, Invocation,
    Launcher, LauncherConfig, LeaseAuthority, Readiness, SpawnIntoCgroup,
};
use std::collections::BTreeSet;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;

/// The two `cpu.max` lines the transition runs between, as the shipped defaults
/// render them.
const UNLEASED_CPU_MAX: &str = "25000 100000";
const LEASED_CPU_MAX: &str = "400000 100000";

/// Every `write_leaf` the broker's launcher performed, in order.
type Writes = Arc<Mutex<Vec<(String, String)>>>;

/// Filesystem seam that records what the PRIVILEGED side wrote.
///
/// It is not a stand-in for the broker: the broker, its authentication, its
/// nonce rotation, its invocation binding and `Launcher::fenced_lift`'s fence
/// check are all real here. Only the cgroupfs underneath is recorded rather
/// than mounted, because a task-run Pod's kernel boundary is proven in the
/// privileged lane and cannot be mounted from a unit-test shard.
struct RecordingFs {
    writes: Writes,
    next_fd: RawFd,
    /// Latest value per (fd, file), so `read_leaf` answers what was written.
    files: std::collections::HashMap<(RawFd, String), String>,
}

impl RecordingFs {
    fn new(writes: Writes) -> Self {
        Self {
            writes,
            next_fd: 100,
            files: std::collections::HashMap::new(),
        }
    }
}

impl CgroupFs for RecordingFs {
    fn readiness(&self) -> Result<Readiness, LauncherError> {
        Ok(Readiness {
            mode: CgroupMode::V2,
            root_writable: true,
            owner_uid: unsafe { libc::geteuid() },
            delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
        })
    }
    fn create_direct_child(&mut self, _: &str) -> Result<RawFd, LauncherError> {
        self.next_fd += 1;
        Ok(self.next_fd)
    }
    fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), LauncherError> {
        self.writes
            .lock()
            .unwrap()
            .push((file.to_owned(), value.to_owned()));
        self.files.insert((fd, file.to_owned()), value.to_owned());
        Ok(())
    }
    fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, LauncherError> {
        if file == "cgroup.events" {
            return Ok("populated 0".to_owned());
        }
        Ok(self
            .files
            .get(&(fd, file.to_owned()))
            .cloned()
            .unwrap_or_else(|| "usage_usec 0".to_owned()))
    }
    fn remove_leaf(&mut self, _: RawFd, _: &str) -> Result<(), LauncherError> {
        Ok(())
    }
}

/// Spawn seam that records the [`Invocation`] the leaf was BORN with, so the
/// birth fence is observable independently of what the lift later presents.
struct RecordingSpawn {
    born: Arc<Mutex<Vec<Invocation>>>,
}

impl SpawnIntoCgroup for RecordingSpawn {
    fn spawn_into_cgroup(
        &mut self,
        _: RawFd,
        invocation: &Invocation,
        _: &djinn_cgroup_launcher::CommandSpec,
    ) -> Result<ChildProcess, LauncherError> {
        self.born.lock().unwrap().push(invocation.clone());
        Ok(ChildProcess {
            pid: 1,
            stdout: -1,
            stderr: -1,
        })
    }
}

/// Dumpability double. The real `prctl(PR_SET_DUMPABLE, 0)` is process-wide and
/// this binary runs many unrelated tests in the same process; the privileged
/// lane exercises the real seam.
struct TestDumpability;
impl WorkerDumpability for TestDumpability {
    fn set_non_dumpable(&mut self) -> Result<(), LauncherError> {
        Ok(())
    }
    fn get_dumpable(&mut self) -> Result<i32, LauncherError> {
        Ok(0)
    }
}

const CREDENTIAL: &[u8] = b"broker-lift-test-credential";

fn broker(
    writes: Writes,
    born: Arc<Mutex<Vec<Invocation>>>,
) -> Broker<RecordingFs, RecordingSpawn> {
    let launcher = Launcher::new(
        RecordingFs::new(writes),
        RecordingSpawn { born },
        LauncherConfig::new(None, None, unsafe { libc::geteuid() }).expect("launcher config"),
    )
    .expect("launcher");
    Broker::new(
        launcher,
        BrokerConfig {
            worker_pid: std::process::id(),
            worker_uid: unsafe { libc::geteuid() },
            worker_gid: unsafe { libc::getegid() },
            pod_credential: CREDENTIAL.to_vec(),
        },
        OsNonceSource,
    )
    .expect("broker")
}

fn connected_client(stream: UnixStream) -> UnixBrokerClient {
    let mut client =
        UnixBrokerClient::connect(stream, CREDENTIAL).expect("the broker must authenticate us");
    client
        .ready(prepare_worker_readiness(&mut TestDumpability).expect("readiness"))
        .expect("READY");
    client
}

fn identity() -> TaskInvocationLeaseIdentity {
    TaskInvocationLeaseIdentity {
        task_id: "task".to_owned(),
        task_run_id: "run".to_owned(),
        invocation_id: uuid::Uuid::now_v7().to_string(),
    }
}

fn shell_command() -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg("true").current_dir("/workspace");
    command
}

/// THE regression test.
///
/// The real production composition — `UnixBrokerLauncher` -> `UnixBrokerClient`
/// -> Unix socket -> `UnixBrokerServer` -> `Broker` -> `Launcher` — must be able
/// to lift the leaf it just created. Before the fix this fails at
/// `handle.fenced_lift()` with the broker's refusal, because the `BEGIN` that
/// bore the leaf named a different fence than the `LIFT`.
#[test]
fn the_production_composition_can_actually_lift_the_leaf_it_created() {
    let writes: Writes = Arc::new(Mutex::new(Vec::new()));
    let born = Arc::new(Mutex::new(Vec::new()));
    let (client_stream, server_stream) = UnixStream::pair().expect("socketpair");

    std::thread::scope(|scope| {
        let mut server = UnixBrokerServer::new(broker(writes.clone(), born.clone()));
        let served = scope.spawn(move || server.serve_connection(server_stream));

        let launcher = UnixBrokerLauncher::new(connected_client(client_stream));
        let identity = identity();
        let mut handle = launcher
            .launch(shell_command(), &identity, LeaseAuthority::Armed)
            .expect("the brokered launch must succeed");

        let birth = writes.lock().unwrap().clone();
        assert_eq!(
            birth,
            vec![("cpu.max".to_owned(), UNLEASED_CPU_MAX.to_owned())],
            "an armed leaf must be born clamped at the unleased quota"
        );

        // The whole blocker, in one call. It returned
        // `Launcher(… InvalidControl)` in production and failed the shell tool.
        handle
            .fenced_lift()
            .expect("the broker must ACCEPT the lift for the invocation it just began");

        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![
                ("cpu.max".to_owned(), UNLEASED_CPU_MAX.to_owned()),
                ("cpu.max".to_owned(), LEASED_CPU_MAX.to_owned()),
            ],
            "the accepted lift must have written the leased quota. Production observed ZERO \
             cpu.max transitions across the entire life of every armed invocation"
        );

        // And the birth fence really is the one the composition derives from the
        // invocation identity — the single source that makes the two controls
        // unable to disagree.
        let born = born.lock().unwrap().clone();
        assert_eq!(born.len(), 1);
        assert_eq!(
            born[0].fence,
            crate::process::broker_invocation_fence(&identity.invocation_id),
            "the leaf must be born with the fence derived from its invocation id"
        );
        assert_ne!(
            born[0].fence, 0,
            "a constant zero birth fence is what the composition shipped with; it can never \
             equal a coordinator fencing token, which starts at 1"
        );

        handle.cleanup().expect("cleanup");
        drop(handle);
        drop(launcher);
        served.join().expect("server thread").expect("served");
    });
}

/// The fence check is LIVE, so the test above is not passing vacuously.
///
/// A `LIFT` presenting anything other than the fence the invocation was begun
/// with must be refused — and refused *legibly*. Production's only diagnostic
/// was `InvalidControl`, which is a real and different broker error, so the
/// message pointed away from the fence for the whole investigation.
#[test]
fn a_lift_whose_fence_disagrees_with_the_birth_is_refused_and_says_so() {
    let writes: Writes = Arc::new(Mutex::new(Vec::new()));
    let born = Arc::new(Mutex::new(Vec::new()));
    let (client_stream, server_stream) = UnixStream::pair().expect("socketpair");

    std::thread::scope(|scope| {
        let mut server = UnixBrokerServer::new(broker(writes.clone(), born.clone()));
        let served = scope.spawn(move || server.serve_connection(server_stream));

        let mut client = connected_client(client_stream);
        let id = uuid::Uuid::now_v7().to_string();
        let fence = crate::process::broker_invocation_fence(&id);
        client
            .begin(Invocation {
                id: id.clone(),
                fence,
            })
            .expect("BEGIN");
        client
            .create(
                &id,
                &id,
                LeaseAuthority::Armed,
                &crate::process::command_spec(shell_command()).expect("spec"),
            )
            .expect("CREATE");

        // This is precisely what production sent: the coordinator's durable
        // fencing token (sequence values start at 1) against a leaf born under a
        // different fence.
        let refused = client
            .lift(&id, 1)
            .expect_err("a mismatched fence must be refused");
        assert!(
            matches!(
                refused,
                LauncherError::ControlRejected(ControlRejection::Fence)
            ),
            "the refusal must NAME the fence; production reported this as `InvalidControl`, a \
             different real broker error, and the misattribution cost the investigation. Got \
             {refused:?}"
        );
        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![("cpu.max".to_owned(), UNLEASED_CPU_MAX.to_owned())],
            "a refused lift must not have written a quota"
        );

        // A rejected fence leaves the binding usable, so the correct fence still
        // lifts. That is what makes the failure above about the FENCE and not
        // about the connection having been poisoned.
        client.lift(&id, fence).expect("the matching fence lifts");
        assert_eq!(
            writes.lock().unwrap().last().cloned(),
            Some(("cpu.max".to_owned(), LEASED_CPU_MAX.to_owned()))
        );

        drop(client);
        served.join().expect("server thread").expect("served");
    });
}

/// Two invocations in one pod must not share a fence, or a lift could be applied
/// to the wrong leaf while still "matching".
#[test]
fn distinct_invocations_get_distinct_fences() {
    let fences: std::collections::BTreeSet<u64> = (0..64)
        .map(|_| crate::process::broker_invocation_fence(&uuid::Uuid::now_v7().to_string()))
        .collect();
    assert_eq!(
        fences.len(),
        64,
        "distinct invocation ids must yield distinct fences"
    );
    // Deterministic: the same identity must derive the same fence at BEGIN and
    // at LIFT even if the two are computed independently.
    let id = uuid::Uuid::now_v7().to_string();
    assert_eq!(
        crate::process::broker_invocation_fence(&id),
        crate::process::broker_invocation_fence(&id)
    );
    // Non-uuid ids still separate rather than collapsing to a constant.
    assert_ne!(
        crate::process::broker_invocation_fence("a"),
        crate::process::broker_invocation_fence("b")
    );
}
