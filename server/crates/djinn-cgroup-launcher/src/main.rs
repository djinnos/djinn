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
//! unleased/leased broker quotas. Anything missing/invalid, a cgroup2 mount the
//! launcher cannot establish, a capability it cannot drop, or a delegated cgroup
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
    Error as LauncherError, Launcher, LauncherConfig, NativeCgroupFs, NativeCgroupSpawn,
};

/// Worker→launcher handshake filename (holds the worker PID) written by the
/// worker into the shared IPC mount next to the private credential.
const WORKER_PID_FILE: &str = "worker.pid";
/// Bounded wait for the worker handshake files to appear (100ms × 600 = 60s).
const HANDSHAKE_POLL: Duration = Duration::from_millis(100);
const HANDSHAKE_ATTEMPTS: u32 = 600;

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

    // Prepare the root supplied by the kubelet RuntimeClass before binding the broker.
    Bootstrap::new(&cgroup_root).run()?;

    // Readiness gate 2 — the delegated cgroup root. `NativeCgroupFs::open` runs
    // the full readiness contract (really a cgroup2 filesystem, cgroup-v2 mode,
    // root writable and not group/other-writable, owner == expected uid, exactly
    // the cpu controller delegated) and fails closed, by name, otherwise.
    let fs = NativeCgroupFs::open(&cgroup_root, expected_uid)?;
    let launcher = Launcher::new(
        fs,
        NativeCgroupSpawn,
        LauncherConfig::new(Some(unleased), Some(leased), expected_uid)?,
    )?;

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
