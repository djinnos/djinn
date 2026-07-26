//! The escalation proof taken through the REAL broker protocol.
//!
//! # Why this file exists (goxi launcher blocker 14)
//!
//! `delegated_cpu_lease_lifecycle.rs` proves the lease governs CPU by calling
//! `Launcher::fenced_lift` **directly**. That is one layer above where the
//! production lift actually happens: in a task-run Pod the worker never holds a
//! `Launcher`, it holds a `UnixBrokerClient`, and the lift is a `LIFT` control
//! frame carrying an invocation id, a rotating nonce and a fence — which the
//! privileged broker validates before it touches cgroupfs.
//!
//! That gap is not theoretical. #2627 fixed the in-pod lift *decision* and
//! shipped with a test asserting that `fenced_lift` was called and that the
//! birth authority was `Armed`. Both were true in production. The broker then
//! **refused the control**, every armed invocation ran its whole life pinned at
//! `cpu.max=[25000 100000]` while burning 670,199 and 749,566 usec (2.7x and 3.0x
//! the escalation threshold) with `nr_throttled` climbing to 30, and — because a
//! rejected lift failed the command — the agent's `shell` tool started returning
//! `lease invocation failed: Launcher(… InvalidControl)`.
//!
//! Nothing in the lane could see it, because nothing in the lane spoke the
//! protocol. This file does:
//!
//! 1. the launcher establishes its own delegated cgroup2 root and drops
//!    `CAP_SYS_ADMIN`, exactly as the sidecar does;
//! 2. a **real** [`UnixBrokerServer`] serves a **real** [`UnixBrokerClient`]
//!    over a Unix socket, through `authenticate` / `READY` / `BEGIN` / `CREATE`;
//! 3. the leaf is born at the unleased quota — read back **from the kernel**,
//!    not from a value this test wrote;
//! 4. a `LIFT` carrying the WRONG fence is refused, and the refusal is legible
//!    (`ControlRejection::Fence`, not the indistinguishable `InvalidControl`),
//!    and `cpu.max` has not moved;
//! 5. a `LIFT` carrying the fence the invocation was BEGUN with is accepted, and
//!    `cpu.max` transitions `25000 100000` -> `400000 100000` in the kernel;
//! 6. and the transition is real throughput, not a file write: `cpu.stat` usage
//!    over a wall-clock window multiplies and `nr_throttled` stops advancing.
//!
//! Step 4 is what makes step 5 non-vacuous. A `LIFT` that the broker accepts
//! regardless of the fence would pass step 5 while proving nothing about the
//! contract that broke.
//!
//! # Where it runs
//!
//! uid 0 plus a cgroup-v2 hierarchy it may mount and delegate, so the proof is
//! `#[ignore]`d and executed by the `launcher-kernel-boundary` CI lane.
//! [`the_brokered_lift_lane_is_wired`] is NOT ignored and fails the ordinary
//! shard if that lane stops running this binary.
//!
//! # Why it is its own binary
//!
//! `Bootstrap::run` is irreversible and process-wide: once `CAP_SYS_ADMIN` is
//! gone nothing later in the same process can mount. Sharing a process with
//! `delegated_cpu_lease_lifecycle` would make whichever proof ran second fail
//! for a reason that has nothing to do with what it asserts.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use djinn_cgroup_launcher::bootstrap::{Bootstrap, INIT_LEAF};
use djinn_cgroup_launcher::broker::{Broker, BrokerConfig, OsNonceSource};
use djinn_cgroup_launcher::child::{NativeWorkerDumpability, prepare_worker_readiness};
use djinn_cgroup_launcher::transport::{UnixBrokerClient, UnixBrokerServer};
use djinn_cgroup_launcher::{
    CommandSpec, ControlRejection, CpuStat, Error, Invocation, Launcher, LauncherConfig,
    LeaseAuthority, LeasedQuota, NativeCgroupFs, NativeCgroupSpawn, UnleasedQuota,
};

/// The workflow job that must execute the `#[ignore]`d proof below.
const PRIVILEGED_LANE_JOB: &str = "launcher-kernel-boundary";
/// Marker the lane uses to declare how many brokered-lift proofs it expects.
const EXPECTED_PROOFS_KEY: &str = "BROKERED_LIFT_EXPECTED_PROOFS";

const UNLEASED_MILLICORES: u16 = UnleasedQuota::DEFAULT_MILLICORES;
const LEASED_MILLICORES: u32 = LeasedQuota::DEFAULT_MILLICORES;
/// The two `cpu.max` lines the transition runs between. Read back from the
/// kernel either side of the `LIFT`; production measured the first one pinned
/// for the whole life of every armed invocation.
const UNLEASED_CPU_MAX: &str = "25000 100000";
const LEASED_CPU_MAX: &str = "400000 100000";
/// Wall-clock window each throughput measurement is taken over.
const WINDOW: Duration = Duration::from_secs(2);
/// Busy loops the probe runs; two, so a lifted leaf can exceed one core.
const SPINNERS: u64 = 2;
/// The fence this invocation is BEGUN with, and the only value its `LIFT` may
/// carry. Deliberately not zero and not one: production sent a hard-coded `0` at
/// `BEGIN` and the coordinator's `build_lease_fencing_token_seq` value (which
/// starts at 1) at `LIFT`, so a proof using either would pass by coincidence.
const INVOCATION_FENCE: u64 = 0x005e_ed0f_1ce5_u64;
/// A fence the invocation was never begun with.
const WRONG_FENCE: u64 = INVOCATION_FENCE ^ 1;

// ═══════════════════ always-on: the lane cannot silently skip ════════════════

/// The privileged lane must run THIS binary, with `--ignored`, without
/// swallowing its failure, and must declare how many proofs it expects.
#[test]
fn the_brokered_lift_lane_is_wired() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/brokered_lease_lift_boundary.rs"),
    )
    .expect("read this test file to count its privileged proofs");
    // Only attributes at the start of a line; prose mentions are backticked.
    let declared = source.matches("\n#[ignore").count();
    assert!(
        declared > 0,
        "this file must declare a privileged proof; an empty suite proves nothing"
    );

    let lane = privileged_lane_block();
    assert!(
        lane.contains("--test brokered_lease_lift_boundary"),
        "the privileged lane must run THIS test binary. It is the only proof that the broker \
         ACCEPTS the lift control — #2627 shipped a green test that asserted only that \
         `fenced_lift` was called, and the broker refused it in production"
    );
    assert!(
        !lane.contains("continue-on-error"),
        "the privileged lane may not swallow its own failure"
    );
    assert!(
        lane.contains(&format!("{EXPECTED_PROOFS_KEY}: \"{declared}\"")),
        "the lane must declare `{EXPECTED_PROOFS_KEY}: \"{declared}\"` so a run that executes \
         fewer than the {declared} declared proofs fails instead of passing"
    );
}

/// The `cpu.max` lines asserted against the kernel below are the ones the
/// launcher actually writes, so a quota default change cannot leave this proof
/// measuring numbers nobody uses.
#[test]
fn the_asserted_cpu_max_lines_are_the_ones_the_launcher_writes() {
    assert_eq!(UNLEASED_MILLICORES, 250);
    assert_eq!(LEASED_MILLICORES, 4_000);
    assert_eq!(
        format!("{} 100000", u64::from(UNLEASED_MILLICORES) * 100_000 / 1000),
        UNLEASED_CPU_MAX
    );
    assert_eq!(
        format!("{} 100000", u64::from(LEASED_MILLICORES) * 100_000 / 1000),
        LEASED_CPU_MAX
    );
    LauncherConfig::new(Some(UNLEASED_MILLICORES), Some(LEASED_MILLICORES), 0)
        .expect("the measured configuration must be one the launcher accepts");
}

/// The refusal the worker sees must name the fence, not the catch-all.
///
/// Production's entire diagnostic was
/// `Launcher(Custom { kind: Other, error: InvalidControl })` for what was really
/// a fence mismatch — and `InvalidControl` is a real, different broker error, so
/// the message actively pointed away from the defect.
#[test]
fn a_refused_lift_is_distinguishable_from_every_other_refusal() {
    assert_eq!(
        ControlRejection::of(&Error::FenceMismatch),
        ControlRejection::Fence
    );
    assert_ne!(
        ControlRejection::of(&Error::FenceMismatch),
        ControlRejection::of(&Error::InvalidControl),
    );
    assert_ne!(
        ControlRejection::of(&Error::FenceMismatch),
        ControlRejection::of(&Error::LiftWithoutAuthority),
    );
}

// ═════════ privileged proof: the BROKER accepts the lift, kernel-measured ════

/// Bootstrap, serve the real protocol, and watch `cpu.max` move in the kernel.
#[ignore = "privileged: needs uid 0 and a cgroup-v2 hierarchy it may mount and delegate \
            (CI job launcher-kernel-boundary)"]
#[test]
fn the_brokered_lift_control_moves_cpu_max_from_unleased_to_leased() {
    require_root();
    let root = scratch_dir("brokered-lift");

    // ── 1. The launcher establishes its own delegated root ──────────────────
    Bootstrap::new(&root).run().unwrap_or_else(|error| {
        panic!(
            "the launcher could not establish its delegated cgroup root at {}: {error}",
            root.display()
        )
    });
    assert!(
        root.join(INIT_LEAF).is_dir(),
        "bootstrap must vacate the delegated root into an init leaf"
    );

    // ── 2. The real broker, over the real filesystem and spawn seams ────────
    let launcher = Launcher::new(
        NativeCgroupFs::open(&root, 0).expect("the established root must satisfy readiness"),
        NativeCgroupSpawn,
        LauncherConfig::new(Some(UNLEASED_MILLICORES), Some(LEASED_MILLICORES), 0)
            .expect("launcher config"),
    )
    .expect("launcher");
    let broker = Broker::new(
        launcher,
        BrokerConfig {
            // The socketpair's peer is this very process, so the broker's
            // `SO_PEERCRED` check is exercised for real against real
            // credentials rather than stubbed out.
            worker_pid: std::process::id(),
            worker_uid: unsafe { libc::geteuid() },
            worker_gid: unsafe { libc::getegid() },
            pod_credential: b"brokered-lift-proof-credential".to_vec(),
        },
        OsNonceSource,
    )
    .expect("broker");

    let (client_stream, server_stream) = UnixStream::pair().expect("control socketpair");
    let leaf = "brokered-lift";
    thread::scope(|scope| {
        let mut server = UnixBrokerServer::new(broker);
        let served = scope.spawn(move || server.serve_connection(server_stream));

        let mut client =
            UnixBrokerClient::connect(client_stream, b"brokered-lift-proof-credential")
                .expect("the broker must authenticate this peer and credential");
        client
            // The real `prctl` seam, not a double: the broker's readiness gate
            // is satisfied exactly the way the worker satisfies it.
            .ready(
                prepare_worker_readiness(&mut NativeWorkerDumpability).expect("worker readiness"),
            )
            .expect("READY");
        client
            .begin(Invocation {
                id: leaf.to_owned(),
                fence: INVOCATION_FENCE,
            })
            .expect("BEGIN");
        client
            .create(leaf, leaf, LeaseAuthority::Armed, &spinner_command())
            .expect("CREATE must make the leaf and spawn the probe into it");

        // ── 3. Born clamped — read from the KERNEL ──────────────────────────
        let cpu_max = root.join(leaf).join("cpu.max");
        assert_eq!(
            read_trimmed(&cpu_max),
            UNLEASED_CPU_MAX,
            "an armed leaf must be born at the unleased quota"
        );
        let unleased = measure(&mut client, leaf, WINDOW);

        // ── 4. A wrong fence is REFUSED, legibly, and moves nothing ─────────
        let refused = client.lift(leaf, WRONG_FENCE).expect_err(
            "a LIFT carrying a fence this invocation was never begun with must be refused; if it \
             is accepted, step 5 below proves nothing about the fencing contract",
        );
        assert!(
            matches!(refused, Error::ControlRejected(ControlRejection::Fence)),
            "the refusal must name the FENCE. Production reported this exact rejection as \
             `InvalidControl` — a different, real broker error — and the misattribution is why \
             the defect survived a release. Got: {refused:?}"
        );
        assert_eq!(
            read_trimmed(&cpu_max),
            UNLEASED_CPU_MAX,
            "a refused lift must not have written a quota"
        );

        // ── 5. The matching fence lifts, measured on the kernel's cpu.max ───
        client
            .lift(leaf, INVOCATION_FENCE)
            .expect("the LIFT carrying the fence the invocation was BEGUN with must be accepted");
        assert_eq!(
            read_trimmed(&cpu_max),
            LEASED_CPU_MAX,
            "the brokered lift must move cpu.max off the unleased quota. This is the assertion \
             production failed: cpu.max stayed `{UNLEASED_CPU_MAX}` for the whole life of every \
             armed invocation, with zero transitions observed over a 2s sampling loop"
        );

        // ── 6. …and it is throughput, not a file write ──────────────────────
        let lifted = measure(&mut client, leaf, WINDOW);
        let unleased_ceiling = quota_usec(u32::from(UNLEASED_MILLICORES), lifted.wall);
        assert!(
            lifted.usage_usec > unleased_ceiling * 2,
            "after the brokered lift the leaf burned {} usec against {} unleased over comparable \
             windows ({:?} / {:?}). A lift the kernel does not honour is a lift in name only",
            lifted.usage_usec,
            unleased.usage_usec,
            lifted.wall,
            unleased.wall
        );
        assert_eq!(
            lifted.nr_throttled, 0,
            "the lifted leaf was throttled {} more times; {SPINNERS} busy loops cannot saturate \
             a {LEASED_MILLICORES}m lease, so any throttling means the quota did not move",
            lifted.nr_throttled
        );
        assert!(
            unleased.nr_throttled > 0,
            "the unleased leaf was never throttled, so the 250m quota was not biting and the \
             comparison above describes nothing"
        );

        // ── 7. The one-way contract holds through the broker too ────────────
        let again = client
            .lift(leaf, INVOCATION_FENCE)
            .expect_err("the lift is one-way; a second one must be refused");
        assert!(
            matches!(
                again,
                Error::ControlRejected(ControlRejection::AlreadyLifted)
            ),
            "a repeated lift must be refused as already-applied, got {again:?}"
        );

        // ── Teardown through the broker's own controls ──────────────────────
        client.kill(leaf).expect("cgroup.kill");
        let drained = (0..200).any(|_| {
            if client.wait_empty(leaf).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
            false
        });
        assert!(drained, "the leaf never reached populated 0");
        client.cleanup(leaf).expect("remove the drained leaf");
        drop(client);
        served.join().expect("server thread").expect("served");
    });
}

// ─────────────────────────────── measurement ────────────────────────────────

struct Measured {
    usage_usec: u64,
    nr_throttled: u64,
    wall: Duration,
}

/// Sample `cpu.stat` **through the broker's `SAMPLE` control**, sleep, sample
/// again, and return the delta. Going through the protocol rather than reading
/// the file directly keeps the measurement describing what a worker can
/// actually observe.
fn measure(client: &mut UnixBrokerClient, leaf: &str, window: Duration) -> Measured {
    let before: CpuStat = client.sample(leaf).expect("SAMPLE");
    #[allow(clippy::disallowed_methods)]
    let started = Instant::now();
    std::thread::sleep(window);
    let elapsed = started.elapsed();
    let after: CpuStat = client.sample(leaf).expect("SAMPLE");
    Measured {
        usage_usec: after.usage_usec.saturating_sub(before.usage_usec),
        nr_throttled: after.nr_throttled.saturating_sub(before.nr_throttled),
        wall: elapsed,
    }
}

/// CPU microseconds a `millicores` quota permits over `wall`, capped by how much
/// the probe could possibly consume ([`SPINNERS`] cores).
fn quota_usec(millicores: u32, wall: Duration) -> u64 {
    let wall_usec = wall.as_micros() as u64;
    (wall_usec * u64::from(millicores) / 1000).min(wall_usec * SPINNERS)
}

// ──────────────────────────────── helpers ───────────────────────────────────

fn spinner_command() -> CommandSpec {
    CommandSpec {
        program: "/bin/sh".to_owned(),
        argv: vec![
            "-c".to_owned(),
            "while : ; do : ; done & while : ; do : ; done".to_owned(),
        ],
        cwd: "/workspace".to_owned(),
        environment: vec![],
    }
}

fn require_root() {
    let euid = unsafe { libc::geteuid() };
    assert_eq!(
        euid, 0,
        "the brokered lift proof must run as uid 0 (uid {euid} cannot mount cgroup2). It runs in \
         the `{PRIVILEGED_LANE_JOB}` CI lane; reproduce it locally with: \
         `cargo test -p djinn-cgroup-launcher --test brokered_lease_lift_boundary --no-run` then \
         `docker run --rm --privileged --cgroupns=private -v \"$PWD:$PWD\" ubuntu:24.04 \
         bash -c 'mkdir -p /workspace && exec \"$0\" --ignored --test-threads 1' <the binary>`"
    );
    assert!(
        Path::new("/workspace").is_dir(),
        "/workspace is missing: the rendered CommandSpec cwd must exist before a child can exec"
    );
}

fn scratch_dir(label: &str) -> PathBuf {
    let base = PathBuf::from(format!("/tmp/djinn-goxi-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .to_owned()
}

/// The privileged lane's own YAML block, isolated from sibling jobs.
fn privileged_lane_block() -> String {
    let workflow = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(".github/workflows/quality-gate.yml"),
    )
    .expect("read the CI workflow that must host the privileged lane");
    let header = format!("\n  {PRIVILEGED_LANE_JOB}:\n");
    let start = workflow.find(&header).unwrap_or_else(|| {
        panic!("no `{PRIVILEGED_LANE_JOB}` job: the brokered lift proof would never execute")
    }) + 1;
    let mut block = String::new();
    for (index, line) in workflow[start..].lines().enumerate() {
        let sibling_job =
            line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':');
        if index > 0 && sibling_job {
            break;
        }
        block.push_str(line);
        block.push('\n');
    }
    block
}
