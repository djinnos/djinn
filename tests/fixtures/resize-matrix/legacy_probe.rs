// Example: `println!` IS this binary's product. It runs as the worker container
// of a live task-run Pod and its stdout — read back with `kubectl logs` — is the
// only channel the harness has.
#![allow(clippy::print_stdout)]
//! The PRE-PROTOCOL worker half of the mixed-version matrix's legacy image
//! class (omp4).
//!
//! # Why this file exists at all
//!
//! `djinn-k8s`'s renderer puts the launcher sidecar and the worker container on
//! the SAME image tag. A legacy image is therefore a legacy launcher AND a
//! legacy worker, and pairing a pre-protocol launcher with the CURRENT
//! `governor_probe` is not a combination production can ever produce.
//!
//! It is also a combination that cannot work, which is worth writing down
//! because it was measured rather than assumed. The authority handshake changed
//! the READY payload from a bare 16-byte proof to 17 bytes (proof + protocol
//! frame byte), and the pre-protocol `WorkerReadinessAssertion::from_wire` does
//! `bytes.try_into()` into `[u8; 16]`. A current worker's READY against a
//! pre-protocol launcher is refused as `ControlRejected(Worker)` before any
//! invocation begins. The current crate's own comment says this is deliberate —
//! "the two binaries ride one image, so there is no rolling-skew case to
//! accommodate" — and this file is that statement taken at its word.
//!
//! # How it is built
//!
//! `build.sh` copies this file into `examples/` of the djinn-cgroup-launcher
//! crate inside the THROWAWAY worktree it checks out at
//! `DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT`, and builds it there. It therefore
//! links against the pre-protocol crate and emits a pre-protocol 16-byte READY.
//! Nothing in the repository's own tree is touched by that build.
//!
//! # What it does, and what it decides
//!
//! It performs the byte-for-byte worker handshake the pre-protocol launcher
//! polls for (`<ipc>/worker.pid` plus the credential file at
//! `DJINN_LAUNCHER_CREDENTIAL_PATH`), completes AUTH/READY/BEGIN/CREATE against
//! the UNMODIFIED pre-protocol launcher binary in the rendered sidecar, and runs
//! a real command in the invocation leaf. It decides NOTHING: it holds no cap,
//! reads no database and never chooses its own authorization.
//!
//! # Output contract
//!
//! One `probe.<record> key=value ...` line per event, matching the subset of
//! `governor_probe`'s wire format that
//! `server/tests/task_run_resize_mixed_version.rs` waits on. Keep it stable.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use djinn_cgroup_launcher::child::{NativeWorkerDumpability, prepare_worker_readiness};
use djinn_cgroup_launcher::transport::UnixBrokerClient;
use djinn_cgroup_launcher::{CommandSpec, Invocation, LeaseAuthority};

/// Worker to launcher handshake filename. MUST match `WORKER_PID_FILE` in the
/// launcher binary's `main.rs`; the launcher joins it onto the credential file's
/// parent directory.
const WORKER_PID_FILE: &str = "worker.pid";
/// Size of the worker-private credential, matching `djinn-agent-worker`.
const CREDENTIAL_BYTES: usize = 32;

const POLL: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            println!("probe.fatal error={error}");
            ExitCode::FAILURE
        }
    }
}

fn env_required(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("missing environment variable {key}"))
}

fn env_number(key: &str, default: u64) -> Result<u64, String> {
    match std::env::var(key) {
        Err(_) => Ok(default),
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|_| format!("{key} is not a number: {raw}")),
    }
}

fn write_worker_handshake(credential_path: &Path) -> Result<Vec<u8>, String> {
    let directory = credential_path
        .parent()
        .ok_or_else(|| "the credential path has no parent directory".to_owned())?;
    let mut credential = vec![0_u8; CREDENTIAL_BYTES];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut credential))
        .map_err(|error| format!("read entropy for the worker credential: {error}"))?;
    fs::write(credential_path, &credential)
        .map_err(|error| format!("write {}: {error}", credential_path.display()))?;
    // The PID file LAST: the launcher polls for both and reads them together, so
    // writing the pid first would let it read a half-written credential.
    let pid_file = directory.join(WORKER_PID_FILE);
    fs::write(&pid_file, std::process::id().to_string())
        .map_err(|error| format!("write {}: {error}", pid_file.display()))?;
    Ok(credential)
}

fn run() -> Result<(), String> {
    // The two RENDERED variables, read from the environment the production
    // renderer produced rather than from constants here: if djinn-k8s moves the
    // socket, this probe must move with it or fail loudly.
    let socket = env_required("DJINN_LAUNCHER_SOCKET")?;
    let credential_path = PathBuf::from(env_required("DJINN_LAUNCHER_CREDENTIAL_PATH")?);
    let invocation = env_required("DJINN_PROBE_INVOCATION")?;
    let fence: u64 = env_required("DJINN_PROBE_FENCE")?
        .trim()
        .parse()
        .map_err(|_| "DJINN_PROBE_FENCE is not a u64".to_owned())?;
    let authority = match env_required("DJINN_PROBE_AUTHORITY")?.as_str() {
        "armed" => LeaseAuthority::Armed,
        "unarmed" => LeaseAuthority::Unarmed,
        other => return Err(format!("DJINN_PROBE_AUTHORITY must be armed|unarmed: {other}")),
    };
    let decision_path = PathBuf::from(env_required("DJINN_PROBE_DECISION_PATH")?);
    let workload = env_required("DJINN_PROBE_WORKLOAD")?;
    let hold = Duration::from_secs(env_number("DJINN_PROBE_HOLD_SECONDS", 600)?);

    println!("probe.start invocation={invocation} fence={fence} socket={socket} legacy=true");

    let credential = write_worker_handshake(&credential_path)?;
    println!(
        "probe.handshake pid={} credential_path={}",
        std::process::id(),
        credential_path.display()
    );

    // The launcher binds its socket only after it has read the handshake, so a
    // connect before that is a legitimate not-yet, not a failure.
    let mut client = None;
    for _ in 0..600 {
        match UnixBrokerClient::connect_path(&socket, &credential) {
            Ok(connected) => {
                client = Some(connected);
                break;
            }
            Err(_) => std::thread::sleep(POLL),
        }
    }
    let mut client = client.ok_or_else(|| format!("never connected to {socket}"))?;

    // The pre-protocol readiness assertion: a bare 16-byte proof, with NO
    // protocol byte, because at this revision there is no protocol to declare.
    let assertion = prepare_worker_readiness(&mut NativeWorkerDumpability)
        .map_err(|error| format!("prepare worker readiness: {error:?}"))?;
    client
        .ready(assertion)
        .map_err(|error| format!("READY refused: {error:?}"))?;
    println!("probe.ready protocol=PreProtocol");

    client
        .begin(Invocation {
            id: invocation.clone(),
            fence,
        })
        .map_err(|error| format!("BEGIN refused: {error:?}"))?;

    // A real command on real bytes, in the invocation leaf. `/workspace` is the
    // only cwd `safe_command_path(cwd, true)` accepts, and the renderer mounts
    // it into the launcher container too.
    let command = CommandSpec {
        program: "/bin/sh".to_owned(),
        argv: vec![
            "-c".to_owned(),
            format!("while :; do /usr/bin/sha256sum {workload} >/dev/null || exit 9; done"),
        ],
        cwd: "/workspace".to_owned(),
        environment: Vec::new(),
    };
    client
        .create(&invocation, &invocation, authority, &command)
        .map_err(|error| format!("CREATE refused: {error:?}"))?;
    println!("probe.created leaf={invocation} authority={authority:?}");

    // The harness reads this line, then records `cpu.max` from inside the Pod.
    // Everything above happens without the harness having said anything.
    println!("probe.awaiting_decision path={}", decision_path.display());

    // Hold the leaf alive so the harness can observe it. Bounded, because a
    // probe that never exits turns a failed run into a hung one.
    let stop = decision_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("stop");
    let ticks = hold.as_millis() / POLL.as_millis();
    for _ in 0..ticks {
        if stop.exists() {
            break;
        }
        std::thread::sleep(POLL);
    }

    let _ = client.kill(&invocation);
    println!("probe.done");
    Ok(())
}
