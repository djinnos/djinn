// Example: `println!` IS this binary's product. It runs as the worker container
// of a live task-run Pod and its stdout — read back with `kubectl logs` — is the
// only channel the measurements have. The workspace denies `print_stdout` for
// library and server code; here the lint would deny the output itself.
#![allow(clippy::print_stdout)]
//! In-pod worker that drives the REAL broker protocol so the invocation
//! governor can be measured end to end on a live cluster (fbiy-C1).
//!
//! # Why this exists as a binary, and what it is not
//!
//! `djinn-cgroup-launcher`'s own `tests/brokered_lease_lift_boundary.rs` already
//! proves the birth clamp, the fence refusal and the lift through a real
//! `UnixBrokerServer`/`UnixBrokerClient` pair — but it does so in ONE process on
//! a CI runner, where the test owns both ends of the socket and the delegated
//! cgroup root is one the test prepared itself. That leaves the two facts a
//! live cluster decides untested: whether a `RuntimeClass`-delegated
//! `/sys/fs/cgroup` in a Kueue-admitted Pod satisfies the launcher's readiness
//! contract at all, and whether the quota the governor authorizes is the quota
//! the kernel then enforces on the Pod's own CPU budget.
//!
//! So this binary is the worker half, and ONLY the worker half. It:
//!
//! * performs the byte-for-byte worker handshake the shipped launcher polls for
//!   (`<ipc>/worker.pid` plus the credential file at
//!   `DJINN_LAUNCHER_CREDENTIAL_PATH`) — the same two files
//!   `djinn-agent-worker`'s `launcher_handshake` writes;
//! * connects to the rendered `DJINN_LAUNCHER_SOCKET` and completes
//!   `AUTH`/`READY`/`BEGIN`/`CREATE` against the **unmodified shipped
//!   launcher binary** running in the rendered sidecar;
//! * runs a real command in the invocation leaf and reports the work it
//!   completes;
//! * samples the leaf's `cpu.stat` **through the broker**, i.e. through
//!   `Launcher::sample` reading the kernel, never a value this process wrote;
//! * attempts exactly one `LIFT`, with a fence supplied from outside, and
//!   reports whether the broker accepted it.
//!
//! It decides NOTHING. It does not read a database, it holds no cap, and it
//! never chooses whether a lift is authorized: the fence it presents arrives in
//! a file written by the harness after the harness has asked the real durable
//! governor. That split is the point — a probe that decided its own
//! authorization would be measuring itself.
//!
//! # The workload is a real command
//!
//! `/usr/bin/sha256sum` over a real multi-megabyte file, four concurrent copies,
//! one line of output per completed digest. The digest is genuine work on
//! genuine bytes and its rate is the throughput this file reports; the `sh` loop
//! around it is a driver, not the measurement. Deliberately NOT a spin loop: a
//! burner that consumes CPU without producing countable output cannot
//! distinguish "the quota was lifted" from "the sampler read a bigger number".
//!
//! # Output contract
//!
//! One `probe.<record> key=value ...` line per event on stdout. The harness
//! (`djinn-k8s`'s `tests/kueue_governor_conformance.rs`) parses these off
//! `kubectl logs`. Keep the shape stable; it is a wire format.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use djinn_cgroup_launcher::broker::ChildStatus;
use djinn_cgroup_launcher::child::{NativeWorkerDumpability, prepare_worker_readiness};
use djinn_cgroup_launcher::transport::UnixBrokerClient;
use djinn_cgroup_launcher::{
    CommandSpec, CpuStat, Invocation, LauncherAuthorityProtocol, LeaseAuthority,
};

/// Worker→launcher handshake filename. MUST match `WORKER_PID_FILE` in the
/// launcher binary's `main.rs`; the launcher joins it onto the credential
/// file's parent directory.
const WORKER_PID_FILE: &str = "worker.pid";
/// Size of the worker-private credential, matching `djinn-agent-worker`.
const CREDENTIAL_BYTES: usize = 32;

/// How often the leaf is drained and sampled. Small enough that the child's
/// stdout pipe never becomes the throughput bound, large enough that the
/// sampling itself is not a load.
const POLL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            println!("probe.fatal error={error}");
            ExitCode::FAILURE
        }
    }
}

struct Settings {
    socket: String,
    credential_path: PathBuf,
    invocation: String,
    fence: u64,
    authority: LeaseAuthority,
    decision_path: PathBuf,
    clamp: Duration,
    lifted: Duration,
    workers: usize,
    workload: String,
    hold: Duration,
}

fn env_required(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("missing environment variable {key}"))
}

fn env_number<T: std::str::FromStr>(key: &str, default: T) -> Result<T, String> {
    match std::env::var(key) {
        Err(_) => Ok(default),
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|_| format!("{key} is not a number: {raw}")),
    }
}

fn settings() -> Result<Settings, String> {
    Ok(Settings {
        // The two rendered variables. Read from the environment the production
        // renderer produced, never from a constant here: if `djinn-k8s` moves
        // the socket, this probe must move with it or fail loudly.
        socket: env_required("DJINN_LAUNCHER_SOCKET")?,
        credential_path: PathBuf::from(env_required("DJINN_LAUNCHER_CREDENTIAL_PATH")?),
        invocation: env_required("DJINN_PROBE_INVOCATION")?,
        fence: env_required("DJINN_PROBE_FENCE")?
            .trim()
            .parse()
            .map_err(|_| "DJINN_PROBE_FENCE is not a u64".to_owned())?,
        authority: match env_required("DJINN_PROBE_AUTHORITY")?.as_str() {
            "armed" => LeaseAuthority::Armed,
            "unarmed" => LeaseAuthority::Unarmed,
            other => return Err(format!("DJINN_PROBE_AUTHORITY must be armed|unarmed: {other}")),
        },
        decision_path: PathBuf::from(env_required("DJINN_PROBE_DECISION_PATH")?),
        clamp: Duration::from_secs(env_number("DJINN_PROBE_CLAMP_SECONDS", 12)?),
        lifted: Duration::from_secs(env_number("DJINN_PROBE_LIFTED_SECONDS", 12)?),
        workers: env_number("DJINN_PROBE_WORKERS", 4)?,
        workload: env_required("DJINN_PROBE_WORKLOAD")?,
        hold: Duration::from_secs(env_number("DJINN_PROBE_HOLD_SECONDS", 300)?),
    })
}

/// The real command the invocation leaf runs.
///
/// `sha256sum` on a real file, `workers` copies in parallel, one line of output
/// per completed digest. `>/dev/null` discards the digest text so the counted
/// line is a fixed two bytes — the count is the measurement, the digest is the
/// work.
fn workload_command(settings: &Settings) -> CommandSpec {
    let one = format!(
        "while :; do /usr/bin/sha256sum {} >/dev/null || exit 9; echo U; done",
        settings.workload
    );
    let mut script = String::new();
    for _ in 0..settings.workers {
        script.push_str(&format!("({one}) & "));
    }
    script.push_str("wait");
    CommandSpec {
        program: "/bin/sh".to_owned(),
        argv: vec!["-c".to_owned(), script],
        cwd: "/".to_owned(),
        // Empty on purpose. The launcher derives and OVERRIDES the child's
        // parallelism pins and git trust anchor itself (`crate::env`), and every
        // path this command names is absolute, so nothing has to be forwarded.
        environment: Vec::new(),
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
    // The PID file LAST: the launcher polls for both and reads them together,
    // so writing the pid first would let it read a half-written credential.
    let pid_file = directory.join(WORKER_PID_FILE);
    fs::write(&pid_file, std::process::id().to_string())
        .map_err(|error| format!("write {}: {error}", pid_file.display()))?;
    Ok(credential)
}

/// Counters drained from the leaf in one poll.
#[derive(Default)]
struct Phase {
    units: u64,
    elapsed: Duration,
    first: Option<CpuStat>,
    last: Option<CpuStat>,
    throttle_advances: u32,
}

impl Phase {
    fn record(&mut self, stat: CpuStat) {
        if self.first.is_none() {
            self.first = Some(stat.clone());
        }
        if let Some(previous) = &self.last
            && stat.throttled_usec > previous.throttled_usec
        {
            self.throttle_advances += 1;
        }
        self.last = Some(stat);
    }

    fn throttled_delta(&self) -> u64 {
        match (&self.first, &self.last) {
            (Some(first), Some(last)) => last.throttled_usec.saturating_sub(first.throttled_usec),
            _ => 0,
        }
    }

    fn usage_delta(&self) -> u64 {
        match (&self.first, &self.last) {
            (Some(first), Some(last)) => last.usage_usec.saturating_sub(first.usage_usec),
            _ => 0,
        }
    }

    /// Completed digests per second over the phase's own wall clock.
    fn units_per_second_milli(&self) -> u64 {
        let millis = self.elapsed.as_millis() as u64;
        if millis == 0 {
            return 0;
        }
        self.units.saturating_mul(1_000).saturating_mul(1_000) / millis
    }
}

fn run() -> Result<(), String> {
    let settings = settings()?;
    println!(
        "probe.start invocation={} fence={} authority={:?} socket={} workers={}",
        settings.invocation, settings.fence, settings.authority, settings.socket, settings.workers,
    );

    let credential = write_worker_handshake(&settings.credential_path)?;
    println!(
        "probe.handshake pid={} credential_path={}",
        std::process::id(),
        settings.credential_path.display(),
    );

    // The launcher binds its socket only after it has read the handshake, so a
    // bounded retry here is the ordering, not a workaround.
    let mut client = None;
    for _ in 0..300 {
        match UnixBrokerClient::connect_path(&settings.socket, &credential) {
            Ok(connected) => {
                client = Some(connected);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let mut client = client.ok_or_else(|| {
        format!(
            "the launcher never accepted an authenticated connection on {}",
            settings.socket
        )
    })?;

    // READY carries the worker's own view of the quota-authority protocol. It is
    // read from the SAME variable the launcher reads; a render that set it on
    // one container and not the other is refused here rather than silently
    // producing a launcher that never writes leaf `cpu.max`.
    let protocol = match std::env::var("DJINN_LAUNCHER_AUTHORITY_PROTOCOL") {
        // Absent means `leaf-v1`, exactly as the launcher binary's own
        // `authority_protocol_from_environment` reads it. An unparseable value
        // is a failure, never a default.
        Err(_) => LauncherAuthorityProtocol::LeafV1,
        Ok(raw) => raw
            .parse()
            .map_err(|_| format!("unparseable DJINN_LAUNCHER_AUTHORITY_PROTOCOL: {raw}"))?,
    };
    let assertion = prepare_worker_readiness(&mut NativeWorkerDumpability, protocol)
        .map_err(|error| format!("prepare worker readiness: {error:?}"))?;
    client
        .ready(assertion)
        .map_err(|error| format!("READY refused: {error:?}"))?;
    println!("probe.ready protocol={protocol:?}");

    client
        .begin(Invocation {
            id: settings.invocation.clone(),
            fence: settings.fence,
        })
        .map_err(|error| format!("BEGIN refused: {error:?}"))?;
    let command = workload_command(&settings);
    client
        .create(
            &settings.invocation,
            &settings.invocation,
            settings.authority,
            &command,
        )
        .map_err(|error| format!("CREATE refused: {error:?}"))?;
    println!(
        "probe.created leaf={} authority={:?}",
        settings.invocation, settings.authority
    );

    let mut clamp = Phase::default();
    drain(&mut client, &settings, &mut clamp, settings.clamp, "clamp")?;
    println!(
        "probe.phase phase=clamp units={} elapsed_ms={} usage_usec={} throttled_usec={} throttle_advances={} rate_milli={}",
        clamp.units,
        clamp.elapsed.as_millis(),
        clamp.usage_delta(),
        clamp.throttled_delta(),
        clamp.throttle_advances,
        clamp.units_per_second_milli(),
    );

    // The harness reads this line, records `cpu.max` from inside the pod, and
    // only then writes the decision. Everything above happens without the
    // harness having said anything about authorization.
    println!("probe.awaiting_decision path={}", settings.decision_path.display());
    let decision = await_decision(&settings)?;

    let lift = match decision {
        Decision::Skip => {
            println!("probe.lift_attempt attempted=false");
            None
        }
        Decision::Lift(fence) => {
            let outcome = client.lift(&settings.invocation, fence);
            match &outcome {
                Ok(()) => println!("probe.lift_attempt attempted=true fence={fence} result=accepted"),
                Err(error) => println!(
                    "probe.lift_attempt attempted=true fence={fence} result=refused error={error:?}"
                ),
            }
            Some(outcome.is_ok())
        }
    };

    let mut lifted = Phase::default();
    drain(&mut client, &settings, &mut lifted, settings.lifted, "post")?;
    println!(
        "probe.phase phase=post units={} elapsed_ms={} usage_usec={} throttled_usec={} throttle_advances={} rate_milli={}",
        lifted.units,
        lifted.elapsed.as_millis(),
        lifted.usage_delta(),
        lifted.throttled_delta(),
        lifted.throttle_advances,
        lifted.units_per_second_milli(),
    );

    let ratio_milli = if clamp.units_per_second_milli() == 0 {
        0
    } else {
        lifted
            .units_per_second_milli()
            .saturating_mul(1_000)
            .checked_div(clamp.units_per_second_milli())
            .unwrap_or(0)
    };
    println!(
        "probe.summary lift_accepted={} clamp_rate_milli={} post_rate_milli={} ratio_milli={}",
        lift.map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_owned()),
        clamp.units_per_second_milli(),
        lifted.units_per_second_milli(),
        ratio_milli,
    );

    // Hold the leaf alive so the harness can read `cpu.max` out of the launcher
    // container one last time. Bounded, because a probe that never exits turns a
    // failed run into a hung one.
    println!("probe.holding seconds={}", settings.hold.as_secs());
    let stop = settings
        .decision_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("stop");
    let held = Instant::now();
    while held.elapsed() < settings.hold && !stop.exists() {
        std::thread::sleep(POLL);
    }

    let _ = client.kill(&settings.invocation);
    println!("probe.done");
    Ok(())
}

enum Decision {
    Lift(u64),
    Skip,
}

/// Block until the harness drops its decision next to the credential mount.
///
/// The file is written by `kubectl exec`; it carries either `skip` or
/// `lift <fence>`. The fence is NOT derived here — deriving it would let the
/// probe authorize itself.
fn await_decision(settings: &Settings) -> Result<Decision, String> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(600) {
        if let Ok(raw) = fs::read_to_string(&settings.decision_path) {
            let text = raw.trim();
            if text == "skip" {
                return Ok(Decision::Skip);
            }
            if let Some(fence) = text.strip_prefix("lift ") {
                return fence
                    .trim()
                    .parse()
                    .map(Decision::Lift)
                    .map_err(|_| format!("decision fence is not a u64: {text}"));
            }
            if !text.is_empty() {
                return Err(format!("unreadable decision: {text}"));
            }
        }
        std::thread::sleep(POLL);
    }
    Err("the harness never wrote a lift decision".to_owned())
}

/// Drain the child's output and sample the leaf for `window`.
fn drain(
    client: &mut UnixBrokerClient,
    settings: &Settings,
    phase: &mut Phase,
    window: Duration,
    label: &str,
) -> Result<(), String> {
    let started = Instant::now();
    while started.elapsed() < window {
        // Drain until the pipe is empty, so the child never blocks on a full
        // pipe — that would make this loop, not the cgroup quota, the bound.
        // Counting `\n` across chunks needs no reassembly: every completed
        // digest contributes exactly one newline wherever the chunk boundary
        // falls.
        loop {
            let (bytes, _eof, status) = client
                .stdout(&settings.invocation)
                .map_err(|error| format!("OUTPUT refused in phase {label}: {error:?}"))?;
            if let ChildStatus::Exited(code) = status {
                return Err(format!("the workload exited early in phase {label}: {code}"));
            }
            if let ChildStatus::Signaled(signal) = status {
                return Err(format!("the workload was killed in phase {label}: {signal}"));
            }
            let empty = bytes.is_empty();
            phase.units += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
            if empty {
                break;
            }
        }
        let stat = client
            .sample(&settings.invocation)
            .map_err(|error| format!("SAMPLE refused in phase {label}: {error:?}"))?;
        phase.record(stat);
        std::thread::sleep(POLL);
    }
    phase.elapsed = started.elapsed();
    Ok(())
}
