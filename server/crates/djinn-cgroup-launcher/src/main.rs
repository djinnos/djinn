//! Packaged entrypoint for the mandatory cgroup-launcher sidecar.
//!
//! This binary is deliberately thin: it only *composes* the crate's existing,
//! separately-tested primitives (`NativeCgroupFs`, `NativeCgroupSpawn`, `Launcher`,
//! `Broker`, `UnixBrokerServer`) into a fail-closed serve loop. It adds no lease
//! policy and no new syscall surface — the crate's runtime behavior is unchanged;
//! this file is the packaging seam so the launcher ships as a real binary
//! (`/opt/djinn/bin/djinn-cgroup-launcher`) alongside `djinn-agent-worker` in the
//! same per-project image (see `server/crates/djinn-image-builder/src/dockerfile.rs`).
//!
//! Configuration is read from the environment the Job renders
//! (`djinn-k8s::launcher`): the delegated cgroup root, control socket path, the
//! worker-private credential path, the expected delegated-root owner uid, and the
//! unleased/leased broker quotas. Anything missing/invalid or a delegated cgroup
//! that fails the [`Readiness`](djinn_cgroup_launcher::Readiness) contract, exits
//! non-zero with a NAMED error BEFORE the broker accepts a single connection.
//! Every one of those conditions is a readiness failure, never a per-command
//! error discovered after the pod has taken work (task grkq).

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use djinn_cgroup_launcher::bootstrap::Bootstrap;
use djinn_cgroup_launcher::broker::{Broker, BrokerConfig, OsNonceSource};
use djinn_cgroup_launcher::transport::UnixBrokerServer;
use djinn_cgroup_launcher::{
    Error as LauncherError, Launcher, LauncherAuthorityProtocol, LauncherConfig, NativeCgroupFs,
    NativeCgroupSpawn,
};

/// Worker→launcher handshake filename (holds the worker PID) written by the
/// worker into the shared IPC mount next to the private credential.
const WORKER_PID_FILE: &str = "worker.pid";
/// Bounded wait for the worker handshake files to appear (100ms × 600 = 60s).
const HANDSHAKE_POLL: Duration = Duration::from_millis(100);
const HANDSHAKE_ATTEMPTS: u32 = 600;

/// Selects who owns leaf CPU quota for this launcher's whole lifetime.
///
/// Absent means `leaf-v1` — the behavior that predates the protocol existing.
/// Anything else that is not a wire form is a readiness failure, not a default;
/// see [`authority_protocol_from_environment`].
const AUTHORITY_PROTOCOL_ENV: &str = "DJINN_LAUNCHER_AUTHORITY_PROTOCOL";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Not `eprintln!`: the `print_stderr` lint is denied workspace-wide.
            let _ = writeln!(std::io::stderr(), "djinn-cgroup-launcher: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), MainError> {
    // Accept `serve` (the rendered arg) or no subcommand; reject anything else.
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => {}
        Some(other) => return Err(MainError::UnknownSubcommand(other.to_string())),
    }
    serve()
}

/// Everything downstream of argument parsing: read the environment, pass every
/// readiness gate, bind the control socket, serve.
///
/// Split from [`run`] only so the readiness ordering is testable — a test
/// binary's own `argv` is not `serve`, and the ordering claim ("an unparseable
/// protocol exits before the socket exists") can only be proven by calling the
/// real function and then looking for the socket.
fn serve() -> Result<(), MainError> {
    let socket = env_required("DJINN_LAUNCHER_SOCKET")?;
    let cgroup_root = env_required("DJINN_LAUNCHER_CGROUP_ROOT")?;
    let credential_path = env_required("DJINN_LAUNCHER_CREDENTIAL_PATH")?;
    let expected_uid: u32 = env_required("DJINN_LAUNCHER_EXPECTED_UID")?
        .parse()
        .map_err(|_| MainError::InvalidEnv("DJINN_LAUNCHER_EXPECTED_UID"))?;
    let unleased: u16 = env_required("DJINN_LAUNCHER_UNLEASED_MILLICORES")?
        .parse()
        .map_err(|_| MainError::InvalidEnv("DJINN_LAUNCHER_UNLEASED_MILLICORES"))?;
    let leased: u32 = env_required("DJINN_LAUNCHER_LEASED_MILLICORES")?
        .parse()
        .map_err(|_| MainError::InvalidEnv("DJINN_LAUNCHER_LEASED_MILLICORES"))?;

    // Readiness gate 1 — the quota authority. Built HERE, with the rest of the
    // environment, and deliberately before `Bootstrap`, before the delegated
    // root is opened and before the control socket is bound: a launcher that
    // cannot name its own protocol must never become reachable. This is the
    // ONLY place the shipped binary learns it, which is the whole point of
    // task 78v1 — #2800 landed the resize-v2 branches with no caller outside
    // the crate's own tests, so the mechanism could not fire in production.
    let config = launcher_config(unleased, leased, expected_uid)?;

    // Prepare the root supplied by the kubelet RuntimeClass before binding the broker.
    Bootstrap::new(&cgroup_root).run()?;

    // Readiness gate 2 — the delegated cgroup root. `NativeCgroupFs::open` runs
    // the full readiness contract (really a cgroup2 filesystem, cgroup-v2 mode,
    // root writable and not group/other-writable, owner == expected uid, exactly
    // the cpu controller delegated) and fails closed, by name, otherwise.
    let fs = NativeCgroupFs::open(&cgroup_root, expected_uid)?;
    let launcher = Launcher::new(fs, NativeCgroupSpawn, config)?;

    // Readiness gate 4 — the git trust anchor. A brokered child runs as a uid
    // that owns none of the pod's volumes, so without a protected-scope
    // `safe.directory` rule EVERY git command it runs dies "detected dubious
    // ownership". The rule has to be a launcher-owned config file, both because
    // the environment form is an arbitrary-config injection primitive and
    // because git strips it from the inner child of `git clone --local` (nurw).
    // Establishing it here means a launcher that cannot write its own anchor
    // fails readiness instead of taking work and then failing every git command
    // in it (task grkq). See `djinn_cgroup_launcher::git_trust`.
    djinn_cgroup_launcher::git_trust::anchor_path()?;

    // Read the worker handshake (private credential + PID) the worker writes into
    // the shared IPC mount at startup. The broker binds every control to that
    // authenticated (pid, credential) pair.
    let (worker_pid, credential) = read_worker_handshake(&credential_path)?;
    let broker = Broker::new(
        launcher,
        BrokerConfig::worker(worker_pid, credential)?,
        OsNonceSource,
    )?;

    let mut server = UnixBrokerServer::bind(&socket, broker)?;
    // One connection at a time: the broker is scoped to a single authenticated
    // worker for the pod's lifetime.
    loop {
        server.serve_once()?;
    }
}

/// Poll for the worker-private credential and PID files, then read them.
/// Fails closed if the worker never completes the handshake within the window.
fn read_worker_handshake(credential_path: &str) -> Result<(u32, Vec<u8>), MainError> {
    let credential = Path::new(credential_path);
    let pid_file = credential
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(WORKER_PID_FILE);

    for _ in 0..HANDSHAKE_ATTEMPTS {
        if credential.exists() && pid_file.exists() {
            let bytes = std::fs::read(credential)?;
            let pid_text = std::fs::read_to_string(&pid_file)?;
            let worker_pid: u32 = pid_text
                .trim()
                .parse()
                .map_err(|_| MainError::InvalidHandshake)?;
            if bytes.is_empty() || worker_pid == 0 {
                return Err(MainError::InvalidHandshake);
            }
            return Ok((worker_pid, bytes));
        }
        std::thread::sleep(HANDSHAKE_POLL);
    }
    Err(MainError::HandshakeTimeout)
}

fn env_required(key: &'static str) -> Result<String, MainError> {
    std::env::var(key).map_err(|_| MainError::MissingEnv(key))
}

/// The launcher configuration the **shipped binary** runs on.
///
/// Quotas come from the caller (they are already parsed and named), the quota
/// authority comes from the process environment. Kept as one named function so
/// there is a single place the binary's config is assembled — and so the proof
/// that resize-v2 is reachable can exercise *this*, not a test-constructed
/// `LauncherConfig`.
fn launcher_config(
    unleased: u16,
    leased: u32,
    expected_uid: u32,
) -> Result<LauncherConfig, MainError> {
    Ok(
        LauncherConfig::new(Some(unleased), Some(leased), expected_uid)?
            .with_authority_protocol(authority_protocol_from_environment()?),
    )
}

/// Read [`AUTHORITY_PROTOCOL_ENV`] from the process environment.
///
/// Three outcomes, and the third is the load-bearing one:
///
/// * **Absent** → [`LauncherAuthorityProtocol::LeafV1`]. Every launcher that
///   predates this variable keeps its exact behavior, so rolling the binary out
///   ahead of the Job template change is a no-op.
/// * **A wire form** → that protocol.
/// * **Anything else** (including non-UTF-8) → an error, which `run` returns
///   before `Bootstrap`, before the delegated root is opened and before the
///   socket exists.
///
/// There is deliberately no `unwrap_or_default`. `resize-v2` misspelled as
/// `resize_v2` would silently become `leaf-v1`, and the launcher would then
/// write leaf `cpu.max` under a Pod whose quota is owned by resize — the exact
/// double-authority the protocol exists to prevent. A pod that cannot say which
/// regime it is in must not take work.
fn authority_protocol_from_environment() -> Result<LauncherAuthorityProtocol, MainError> {
    match std::env::var(AUTHORITY_PROTOCOL_ENV) {
        Ok(raw) => raw
            .parse()
            .map_err(|_| MainError::InvalidEnv(AUTHORITY_PROTOCOL_ENV)),
        Err(std::env::VarError::NotPresent) => Ok(LauncherAuthorityProtocol::LeafV1),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(MainError::InvalidEnv(AUTHORITY_PROTOCOL_ENV))
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum MainError {
    #[error("unknown subcommand: {0}")]
    UnknownSubcommand(String),
    #[error("required environment variable {0} is not set")]
    MissingEnv(&'static str),
    #[error("environment variable {0} is malformed")]
    InvalidEnv(&'static str),
    #[error("worker handshake files were malformed")]
    InvalidHandshake,
    #[error("worker handshake did not complete in time")]
    HandshakeTimeout,
    #[error(transparent)]
    Launcher(#[from] LauncherError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The reachability proofs for the shipped binary.
///
/// # Why these live in the binary and not in the library
///
/// #2800 added the `resize-v2` branches, a `with_authority_protocol` setter and
/// a library test that drove the setter directly. All of it passed, including
/// the privileged Launcher Kernel Boundary lane, and the mechanism still could
/// not fire in production: `LauncherConfig::new` hard-coded `LeafV1`, the setter
/// had three callers and all three were tests, and no environment variable
/// reached it. A library test cannot detect that, because a library test is
/// allowed to construct the config however it likes.
///
/// So these tests deliberately start from the *process environment* and call
/// the binary's own [`launcher_config`] and [`serve`]. Revert `main.rs` to
/// `LauncherConfig::new(...)` with no setter call and
/// `resize_v2_is_reachable_from_the_process_environment_alone` fails on its
/// first `cpu.max` write.
#[cfg(test)]
mod tests {
    use super::*;
    use djinn_cgroup_launcher::{
        CgroupFs, CgroupMode, ChildProcess, CommandSpec, Error, Invocation, LeaseAuthority,
        Readiness, SpawnIntoCgroup,
    };
    use std::collections::{BTreeSet, HashMap};
    use std::os::fd::RawFd;
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

    const EXPECTED_UID: u32 = 7;

    /// Serializes every test that mutates the process environment.
    ///
    /// `unwrap_or_else(PoisonError::into_inner)` is deliberate: a failing
    /// assertion in one test must not turn its siblings into `PoisonError`
    /// reports, which would hide which assertion actually broke — the exact
    /// confusion that makes mutation testing unreadable.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// RAII owner of [`AUTHORITY_PROTOCOL_ENV`] so a panicking assertion cannot
    /// leak a value into the next test.
    struct ProtocolVar;

    impl ProtocolVar {
        fn set(value: &str) -> Self {
            // SAFETY: every writer of this variable holds `env_lock`, and this
            // process spawns no threads that read the environment concurrently.
            unsafe { std::env::set_var(AUTHORITY_PROTOCOL_ENV, value) };
            Self
        }

        fn unset() -> Self {
            // SAFETY: as above.
            unsafe { std::env::remove_var(AUTHORITY_PROTOCOL_ENV) };
            Self
        }
    }

    impl Drop for ProtocolVar {
        fn drop(&mut self) {
            // SAFETY: as above.
            unsafe { std::env::remove_var(AUTHORITY_PROTOCOL_ENV) };
        }
    }

    /// Records every leaf-file write so "the launcher wrote no quota" is an
    /// assertion about bytes, not about a flag.
    #[derive(Default)]
    struct RecordingFs {
        files: HashMap<(RawFd, String), String>,
        writes: Vec<(String, String)>,
        next: RawFd,
    }

    impl RecordingFs {
        fn ready() -> Self {
            Self {
                next: 10,
                ..Self::default()
            }
        }
    }

    impl CgroupFs for RecordingFs {
        fn readiness(&self) -> Result<Readiness, Error> {
            Ok(Readiness {
                mode: CgroupMode::V2,
                root_writable: true,
                owner_uid: EXPECTED_UID,
                delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
            })
        }
        fn create_direct_child(&mut self, _: &str) -> Result<RawFd, Error> {
            self.next += 1;
            Ok(self.next)
        }
        fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), Error> {
            self.writes.push((file.to_owned(), value.to_owned()));
            self.files.insert((fd, file.to_owned()), value.to_owned());
            Ok(())
        }
        fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, Error> {
            Ok(self
                .files
                .get(&(fd, file.to_owned()))
                .cloned()
                .unwrap_or_else(|| "populated 0\n".to_owned()))
        }
        fn remove_leaf(&mut self, _: RawFd, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSpawn {
        spawned: usize,
    }

    impl SpawnIntoCgroup for RecordingSpawn {
        fn spawn_into_cgroup(
            &mut self,
            _: RawFd,
            _: &Invocation,
            _: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            self.spawned += 1;
            Ok(ChildProcess {
                pid: 1,
                stdout: -1,
                stderr: -1,
            })
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

    /// Drive one leaf all the way through birth, sample, fenced lift and every
    /// terminal step, and return every `(file, value)` the launcher wrote.
    ///
    /// The config is built by the binary's own [`launcher_config`], so what is
    /// under test is the path the shipped binary takes.
    fn writes_for_a_full_leaf_lifecycle() -> Vec<(String, String)> {
        let config = launcher_config(250, 4_000, EXPECTED_UID)
            .expect("the launcher config must build from this environment");
        let mut launcher = Launcher::new(RecordingFs::ready(), RecordingSpawn::default(), config)
            .expect("the recording fs satisfies the readiness contract");

        let invocation = Invocation {
            id: "reachability".to_owned(),
            fence: 99,
        };
        let (mut leaf, child) = launcher
            .create_command(
                "reachability",
                invocation,
                LeaseAuthority::Armed,
                &command(),
            )
            .expect("the leaf must be created");
        assert_eq!(child.pid, 1, "the child still spawns under either protocol");
        launcher.sample(&leaf).expect("cpu.stat is still readable");
        launcher
            .fenced_lift(&mut leaf, 99)
            .expect("a matching fence must be accepted under either protocol");
        launcher.kill(&mut leaf).expect("kill is protocol-neutral");
        launcher.wait_empty(&leaf).expect("the leaf drains");
        launcher.remove(&leaf).expect("the leaf is removed");

        let (fs, _) = launcher.into_parts();
        fs.writes
    }

    /// **The reachability proof (task 78v1, AC3).**
    ///
    /// Nothing here constructs a `LauncherConfig` by hand. The only difference
    /// between the two halves is one string in the process environment — which
    /// is the only thing a Kubernetes Job can actually change.
    #[test]
    fn resize_v2_is_reachable_from_the_process_environment_alone() {
        let _guard = env_lock();

        let held = ProtocolVar::set("resize-v2");
        let under_resize_v2 = writes_for_a_full_leaf_lifecycle();
        drop(held);

        assert!(
            under_resize_v2.iter().all(|(file, _)| file != "cpu.max"),
            "DJINN_LAUNCHER_AUTHORITY_PROTOCOL=resize-v2 must make ZERO cpu.max writes at \
             birth, lift or teardown; the launcher wrote {under_resize_v2:?}"
        );
        assert_eq!(
            under_resize_v2,
            vec![("cgroup.kill".to_owned(), "1".to_owned())],
            "every containment property other than quota must survive resize-v2"
        );

        let held = ProtocolVar::unset();
        let under_default = writes_for_a_full_leaf_lifecycle();
        drop(held);

        assert_eq!(
            under_default,
            vec![
                ("cpu.max".to_owned(), "25000 100000".to_owned()),
                ("cpu.max".to_owned(), "400000 100000".to_owned()),
                ("cgroup.kill".to_owned(), "1".to_owned()),
            ],
            "with the variable absent the launcher must keep writing the leaf-v1 birth quota \
             and the fenced lift; a resize-v2 assertion that also holds here proves nothing"
        );
    }

    /// The explicit `leaf-v1` spelling is the same regime as absence — a Job
    /// template that names the current behavior must not change it.
    #[test]
    fn an_explicit_leaf_v1_is_identical_to_an_absent_variable() {
        let _guard = env_lock();
        let held = ProtocolVar::set("leaf-v1");
        let explicit = writes_for_a_full_leaf_lifecycle();
        drop(held);
        let held = ProtocolVar::unset();
        let absent = writes_for_a_full_leaf_lifecycle();
        drop(held);
        assert_eq!(explicit, absent);
        assert_eq!(explicit[0].0, "cpu.max");
    }

    /// **AC4.** An unparseable protocol is a readiness failure, and readiness
    /// failures happen before the launcher is reachable.
    ///
    /// The socket path is asserted absent afterwards, so `unwrap_or_default()`
    /// cannot pass this by "succeeding" — it would return a *different* error
    /// (the temp dir is not a cgroup2 filesystem), which the variant assertion
    /// catches, and any future reordering that binds first is caught by the
    /// path assertion.
    #[test]
    fn an_unparseable_protocol_exits_non_zero_before_the_socket_is_bound() {
        let _guard = env_lock();

        let directory = std::env::temp_dir().join(format!(
            "djinn-launcher-protocol-readiness-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("the temp directory must be creatable");
        let socket = directory.join("launcher.sock");
        let _ = std::fs::remove_file(&socket);

        // SAFETY: `env_lock` is held; no other thread reads the environment.
        unsafe {
            std::env::set_var("DJINN_LAUNCHER_SOCKET", &socket);
            std::env::set_var("DJINN_LAUNCHER_CGROUP_ROOT", &directory);
            std::env::set_var("DJINN_LAUNCHER_CREDENTIAL_PATH", directory.join("cred"));
            std::env::set_var("DJINN_LAUNCHER_EXPECTED_UID", "7");
            std::env::set_var("DJINN_LAUNCHER_UNLEASED_MILLICORES", "250");
            std::env::set_var("DJINN_LAUNCHER_LEASED_MILLICORES", "4000");
        }

        for unparseable in ["resize-v3", "leafv1", "RESIZE-V2", "", " resize-v2"] {
            let held = ProtocolVar::set(unparseable);
            let outcome = serve();
            drop(held);

            match outcome {
                Err(MainError::InvalidEnv(key)) => assert_eq!(key, AUTHORITY_PROTOCOL_ENV),
                other => panic!(
                    "{unparseable:?} must be refused by name before anything else is attempted, \
                     got {other:?}"
                ),
            }
            assert!(
                !socket.exists(),
                "{unparseable:?} must be refused BEFORE the control socket exists"
            );
        }

        // SAFETY: `env_lock` is held.
        unsafe {
            for key in [
                "DJINN_LAUNCHER_SOCKET",
                "DJINN_LAUNCHER_CGROUP_ROOT",
                "DJINN_LAUNCHER_CREDENTIAL_PATH",
                "DJINN_LAUNCHER_EXPECTED_UID",
                "DJINN_LAUNCHER_UNLEASED_MILLICORES",
                "DJINN_LAUNCHER_LEASED_MILLICORES",
            ] {
                std::env::remove_var(key);
            }
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Absence is `leaf-v1` and a wire form is itself — asserted on the exact
    /// function the binary calls, so a future refactor that stops consulting
    /// the environment is caught here as well as by the lifecycle proof.
    #[test]
    fn the_environment_read_is_absent_means_leaf_v1_and_nothing_else_is_accepted() {
        let _guard = env_lock();

        let held = ProtocolVar::unset();
        assert_eq!(
            authority_protocol_from_environment().unwrap(),
            LauncherAuthorityProtocol::LeafV1
        );
        drop(held);

        for (value, expected) in [
            ("leaf-v1", LauncherAuthorityProtocol::LeafV1),
            ("resize-v2", LauncherAuthorityProtocol::ResizeV2),
        ] {
            let held = ProtocolVar::set(value);
            assert_eq!(authority_protocol_from_environment().unwrap(), expected);
            drop(held);
        }

        for rejected in ["resize-v3", "leaf_v1", "Leaf-V1", "resize-v2 ", "none"] {
            let held = ProtocolVar::set(rejected);
            let outcome = authority_protocol_from_environment();
            drop(held);
            assert!(
                matches!(outcome, Err(MainError::InvalidEnv(key)) if key == AUTHORITY_PROTOCOL_ENV),
                "{rejected:?} must be refused, got {outcome:?}"
            );
        }
    }
}
