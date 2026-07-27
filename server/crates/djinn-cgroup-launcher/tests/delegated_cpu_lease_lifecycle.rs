//! The end-to-end proof that the per-invocation CPU lease actually governs CPU.
//!
//! # Why this file exists (task 7deu)
//!
//! Every previous test of this component asserted *configuration*. The
//! containment suite drives a `FakeCgroup` that hard-codes `root_writable:
//! true` — the exact property that was false in production. The clone seam was
//! faked, so a flag constant that silently truncated to zero (making every
//! "contained" child an ordinary fork in the launcher's own cgroup) passed
//! everything. And the render tests read `cpu.max` back, which reports what was
//! written, not what the kernel enforces: with a 250m limit on the launcher
//! container, a leaf configured for four cores measured 0.25 core and reported
//! `nr_throttled 0`, because the throttling happened at the ancestor.
//!
//! So this file asserts **behaviour, measured over a known wall-clock window**:
//!
//! 1. the launcher establishes its own delegated cgroup2 root by `mount(2)`;
//! 2. it drops `CAP_SYS_ADMIN`/`CAP_SYS_RESOURCE` and **cannot mount again**;
//! 3. a real child, spawned by the production seam, is a member of the
//!    invocation leaf;
//! 4. unleased, it burns *approximately the unleased quota* — two-sided, so it
//!    fails both if the child escaped the cgroup (usage ≈ 0) and if the quota
//!    did not bite (usage ≈ 2 cores), and `nr_throttled` is non-zero, which is
//!    what makes throttle-based heavy detection possible at all;
//! 5. after a fenced lift it burns *multiples more* in the same window and stops
//!    being throttled;
//! 6. a sibling leaf with no child accounts for nothing, so the measurement is
//!    attributing CPU to the right cgroup;
//! 7. a leaf created under an UNARMED lease authority is never clamped at all —
//!    `cpu.max` reads `max 100000` and it measures multiples of the clamped
//!    leaf, while still containing its child.
//!
//! # What step 7 adds (goxi launcher blocker 11)
//!
//! Steps 4-5 prove the lease *can* govern CPU. They cannot see whether the lease
//! is ever *reachable*. Production armed the launcher while the durable
//! `admission_handoff` row was ABSENT — `djinn-server epoch show` reported
//! `admission handoff row: <absent>` during the armed window — which
//! `evaluate_invocation_lift` maps to `Unleased`, and the runner implemented that
//! as a no-op. The leaf had already been born at 250m, so the "no-op" pinned
//! every build there: a measured leaf reached 21,130,868 usec of CPU, 84x the
//! 250,000 usec escalation threshold, with `cpu.max` never leaving
//! `25000 100000`. Arming was a ~16x slowdown and rolled back four times.
//!
//! Step 7 makes that state measurable: containment without a reachable lift must
//! cost nothing.
//!
//! Nothing here can pass vacuously. There is no `FakeCgroup`, no fake clone, no
//! environment probe that returns "not applicable", and no assertion on a value
//! this crate itself wrote.
//!
//! # Where it runs
//!
//! It needs uid 0 and a real cgroup-v2 hierarchy it may mount and delegate, so
//! the proof is `#[ignore]`d for ordinary unprivileged runs and executed by the
//! `launcher-kernel-boundary` CI lane. [`the_lease_lifecycle_lane_is_wired`] is
//! NOT ignored and fails the ordinary shard if that lane stops running this
//! binary — a proof nobody executes looks exactly like a proof that passed.
//!
//! # Why it is a single proof
//!
//! Step 2 is irreversible: once the launcher has `capset`ed `CAP_SYS_ADMIN`
//! away, nothing later in the same process can mount anything. Splitting this
//! into several `#[test]`s sharing one process would make all but the first
//! fail for a reason that has nothing to do with what they assert.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use djinn_cgroup_launcher::bootstrap::{Bootstrap, INIT_LEAF, holds_any_bootstrap_capability};
use djinn_cgroup_launcher::{
    CommandSpec, CpuStat, Invocation, Launcher, LauncherConfig, LeaseAuthority, LeasedQuota,
    NativeCgroupFs, NativeCgroupSpawn, UnleasedQuota,
};

/// The workflow job that must execute the `#[ignore]`d proof below.
const PRIVILEGED_LANE_JOB: &str = "launcher-kernel-boundary";
/// Marker the lane uses to declare how many lease proofs it expects to execute.
const EXPECTED_PROOFS_KEY: &str = "LEASE_LIFECYCLE_EXPECTED_PROOFS";

/// Wall-clock window each measurement is taken over. Long enough that CFS period
/// boundaries (100ms) average out, short enough to keep the lane quick.
const WINDOW: Duration = Duration::from_secs(2);
/// Unleased quota under test: the shipped default.
const UNLEASED_MILLICORES: u16 = UnleasedQuota::DEFAULT_MILLICORES;
/// Leased quota under test: the shipped default, four cores.
const LEASED_MILLICORES: u32 = LeasedQuota::DEFAULT_MILLICORES;
/// How many busy loops the probe command runs. Two, so that a lifted leaf can
/// demonstrably exceed one core while staying inside a four-core lease.
const SPINNERS: u64 = 2;

/// Checked-in bridge from the production Job renderer to this crate. The
/// `djinn-k8s` contract test rebuilds its default required Job and requires
/// every key in this file to match the manifest; this suite refuses to measure
/// a hand-maintained approximation instead.
const RENDERED_CONTRACT_FIXTURE: &str = "fixtures/rendered-security-context.env";

// ═══════════════════ always-on: the lane cannot silently skip ════════════════

/// The privileged lane must run THIS binary, with `--ignored`, without
/// swallowing its failure, and must declare how many proofs it expects.
#[test]
fn the_lease_lifecycle_lane_is_wired() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/delegated_cpu_lease_lifecycle.rs"),
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
        lane.contains("--test delegated_cpu_lease_lifecycle"),
        "the privileged lane must run THIS test binary, or the only behavioural proof that the \
         lease governs CPU never executes"
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

fn rendered_contract() -> BTreeMap<String, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(RENDERED_CONTRACT_FIXTURE);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read rendered Job contract {}: {error}", path.display()))
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (key, value) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("rendered Job contract line is key=value: {line}"));
            (key.trim().to_owned(), value.trim().to_owned())
        })
        .collect()
}

/// The cpu.stat lane consumes the mandatory Job contract `djinn-k8s` validates
/// against its production/default render. A launcher CPU limit would hide
/// throttling at an ancestor, while an implicit or unlimited lift would make
/// the post-fence measurement describe the wrong Pod.
#[test]
fn the_cpu_stat_lane_consumes_the_rendered_required_job_contract() {
    assert_rendered_required_job_contract();
}

fn assert_rendered_required_job_contract() {
    let contract = rendered_contract();
    for (key, expected) in [
        ("seccomp_profile", "RuntimeDefault"),
        ("launcher_apparmor_profile", "Unconfined"),
        ("worker_allow_privilege_escalation", "false"),
        ("launcher_allow_privilege_escalation", "false"),
        ("launcher_capabilities_drop", "ALL"),
        (
            "launcher_capabilities_add",
            "CHOWN,SETGID,SETUID,SETPCAP,SYS_ADMIN,SYS_RESOURCE",
        ),
        ("launcher_cpu_limit", "none"),
        ("launcher_cpu_request", "50m"),
        ("launcher_memory_request", "64Mi"),
        ("launcher_memory_limit", "4Gi"),
        ("launcher_lease_quota_millicores", "4000"),
    ] {
        assert_eq!(contract.get(key).map(String::as_str), Some(expected));
    }
    assert_eq!(
        contract["unleased_millicores"]
            .parse::<u16>()
            .expect("rendered unleased quota"),
        UNLEASED_MILLICORES,
        "the measured throttle must use the rendered unleased quota"
    );
    assert_eq!(
        contract["leased_millicores"]
            .parse::<u32>()
            .expect("rendered lifted quota"),
        LEASED_MILLICORES,
        "the matching-fence lift must use the rendered explicit lease quota"
    );
    assert_eq!(
        contract["launcher_lease_quota_millicores"], contract["leased_millicores"],
        "the rendered launcher environment and the cpu.stat lift must name one quota"
    );
}

/// The exact `cpu.max` lines the privileged proof reads back, pinned here so a
/// change to a quota default or to the period cannot silently make the kernel
/// assertions describe different numbers than production writes.
#[test]
fn the_asserted_cpu_max_lines_are_the_ones_the_launcher_writes() {
    assert_eq!(
        djinn_cgroup_launcher::unrestricted_cpu_max(),
        "max 100000",
        "an unarmed leaf's line is what step 7 asserts against the kernel"
    );
    // `25000 100000` is the line production measured on a leaf that burned 21.1
    // CPU-seconds and never escalated; `4000000 100000` is what the lift must
    // replace it with.
    assert_eq!(
        u64::from(UNLEASED_MILLICORES) * 100_000 / 1000,
        25_000,
        "the unleased line must be `25000 100000`"
    );
    assert_eq!(
        u64::from(LEASED_MILLICORES) * 100_000 / 1000,
        400_000,
        "the leased line must be `400000 100000`"
    );
}

/// The quotas this proof measures are the ones the launcher crate ships, so a
/// change to either default cannot leave the measurement asserting a number
/// nobody uses.
#[test]
fn the_measured_quotas_are_the_shipped_defaults() {
    assert_eq!(UNLEASED_MILLICORES, 250);
    assert_eq!(LEASED_MILLICORES, 4_000);
    // And the lease must actually be a lift, or every expectation below is
    // meaningless.
    assert!(u32::from(UNLEASED_MILLICORES) < LEASED_MILLICORES);
    LauncherConfig::new(Some(UNLEASED_MILLICORES), Some(LEASED_MILLICORES), 0)
        .expect("the measured configuration must be one the launcher accepts");
}

// ══════════════════ privileged proof: the lease governs CPU ══════════════════

/// Mount, delegate, drop the capability, throttle, lift — all measured.
#[ignore = "privileged: needs uid 0 and a cgroup-v2 hierarchy it may mount and delegate \
            (CI job launcher-kernel-boundary)"]
#[test]
fn the_delegated_lease_throttles_and_lifts_measured_on_cpu_stat() {
    // This is deliberately in the ignored proof too: the CI lane that measures
    // cpu.stat cannot run against a stale security/resource approximation.
    assert_rendered_required_job_contract();
    require_root();
    let root = scratch_dir("lease-lifecycle");

    // ── 1. The launcher establishes its own delegated root ──────────────────
    //
    // This is the step an `emptyDir` could never perform and the reason the
    // sidecar CrashLoopBackOffed on every task-run Pod. It also drops the
    // bootstrap capabilities on the way out.
    Bootstrap::new(&root).run().unwrap_or_else(|error| {
        panic!(
            "the launcher could not establish its delegated cgroup root at {}: {error}. If this \
             is `enable the cpu controller`, the host's parent cgroup does not offer `cpu` to \
             its children and the lease cannot be proven here.",
            root.display()
        )
    });

    // ── 2. …and can no longer mount anything ────────────────────────────────
    assert!(
        !holds_any_bootstrap_capability().expect("read this process's capability set"),
        "CAP_SYS_ADMIN survived bootstrap; a task-run pod holding it has a node-wide escape \
         primitive (/proc/sys/kernel/core_pattern is not namespaced)"
    );
    let second = scratch_dir("post-drop-mount");
    let errno = try_mount_cgroup2(&second);
    assert_eq!(
        errno,
        Some(libc::EPERM),
        "after the capability drop a second cgroup2 mount must fail with EPERM; it returned \
         {errno:?}, so the drop did not take"
    );
    let _ = std::fs::remove_dir_all(&second);

    // The launcher's own process is out of the mount root, which is what the
    // "no internal process" rule requires before `+cpu` can be delegated.
    assert!(
        root.join(INIT_LEAF).is_dir(),
        "bootstrap must vacate the delegated root into an init leaf"
    );
    assert_eq!(
        read_trimmed(&root.join("cgroup.subtree_control")),
        "cpu",
        "the delegated root must enable exactly the cpu controller"
    );
    assert_eq!(
        read_trimmed(&root.join("cgroup.procs")),
        "",
        "the delegated root must hold no processes of its own"
    );

    // ── 3. The real launcher, over the real filesystem seam ─────────────────
    let fs = NativeCgroupFs::open(&root, 0)
        .expect("the root the launcher just established must satisfy its own readiness contract");
    let mut launcher = Launcher::new(
        fs,
        NativeCgroupSpawn,
        LauncherConfig::new(Some(UNLEASED_MILLICORES), Some(LEASED_MILLICORES), 0)
            .expect("launcher config"),
    )
    .expect("launcher");

    let invocation = Invocation {
        id: "lease-lifecycle".to_owned(),
        fence: 0x7de_u64,
    };
    let (mut leaf, child) = launcher
        .create_command(
            "leased",
            invocation,
            LeaseAuthority::Armed,
            &spinner_command(),
        )
        .expect("spawn the probe command into an invocation leaf");

    // A sibling that never gets a child. It is the control for step 6.
    let idle_invocation = Invocation {
        id: "idle".to_owned(),
        fence: 1,
    };
    let (idle_leaf, idle_child) = launcher
        .create_command(
            "idle",
            idle_invocation,
            LeaseAuthority::Armed,
            &sleep_command(),
        )
        .expect("spawn the idle control");

    // ── 4. Membership, then the unleased measurement ────────────────────────
    let members = read_trimmed(&root.join("leased").join("cgroup.procs"));
    assert!(
        members
            .split_ascii_whitespace()
            .any(|entry| entry.parse::<i32>() == Ok(child.pid)),
        "child {} is not a member of the invocation leaf (members: {members:?}); its CPU would \
         be governed by nothing at all",
        child.pid
    );

    let unleased = measure(&mut launcher, &leaf, WINDOW);
    let expected_unleased = quota_usec(u32::from(UNLEASED_MILLICORES), unleased.wall);
    assert!(
        unleased.usage_usec > expected_unleased / 2,
        "the leaf accounted for only {} usec over {:?}; {SPINNERS} busy loops inside a {}m quota \
         must burn roughly {expected_unleased} usec. A near-zero reading means the child is not \
         actually in this cgroup — which is exactly what a silently-truncated clone flag looked \
         like.",
        unleased.usage_usec,
        unleased.wall,
        UNLEASED_MILLICORES
    );
    assert!(
        unleased.usage_usec < expected_unleased * 2,
        "the leaf accounted for {} usec over {:?}, far above the {}m quota's {expected_unleased} \
         usec. The quota is not being enforced — which is what an ancestor CPU limit on the \
         launcher container produced: the leaf said four cores and the kernel gave 0.25.",
        unleased.usage_usec,
        unleased.wall,
        UNLEASED_MILLICORES
    );
    assert!(
        unleased.nr_throttled > 0,
        "the unleased leaf reported nr_throttled 0. Throttling is happening somewhere else — at \
         an ancestor — so throttle-based heavy detection is structurally blind, which is the \
         defect this task exists to remove."
    );

    // ── 5. The fenced lift, measured the same way ───────────────────────────
    //
    // `cpu.max` is read from the KERNEL either side of the lift, not inferred
    // from the measurement. The production symptom of blocker 11 was precisely a
    // quota that never changed: an armed launcher whose leaf sat at
    // `25000 100000` while the child burned 21.1 CPU-seconds, 84x the escalation
    // threshold. So the transition itself is an assertion.
    let cpu_max_path = root.join("leased").join("cpu.max");
    assert_eq!(
        read_trimmed(&cpu_max_path),
        launcher.birth_cpu_max(LeaseAuthority::Armed),
        "an armed leaf must be born at the unleased quota"
    );
    assert_eq!(
        read_trimmed(&cpu_max_path),
        "25000 100000",
        "the birth quota must be the 250m line production measured"
    );
    launcher
        .fenced_lift(&mut leaf, 0x7de_u64)
        .expect("a matching fence must lift the quota");
    assert_eq!(
        read_trimmed(&cpu_max_path),
        "400000 100000",
        "the fenced lift must move cpu.max off the unleased quota; a lift that \
         leaves it at `25000 100000` is the production defect (blocker 11)"
    );

    let leased = measure(&mut launcher, &leaf, WINDOW);
    assert!(
        leased.usage_usec > unleased.usage_usec * 3,
        "after the lift the leaf burned {} usec against {} unleased over comparable windows. A \
         lift that does not multiply throughput is a lift in name only — the ancestor is still \
         clamping.",
        leased.usage_usec,
        unleased.usage_usec
    );
    let ceiling = quota_usec(LEASED_MILLICORES, leased.wall);
    assert!(
        leased.usage_usec <= ceiling,
        "after the lift the leaf burned {} usec, above the {}m lease's {ceiling} usec ceiling \
         over {:?}. The lift must raise the quota, not remove it: an unbounded leaf lets one \
         build take the whole node.",
        leased.usage_usec,
        LEASED_MILLICORES,
        leased.wall
    );
    assert_eq!(
        leased.nr_throttled, 0,
        "{SPINNERS} busy loops cannot saturate a {LEASED_MILLICORES}m lease, so the lifted leaf \
         must stop being throttled entirely"
    );

    // The numbers themselves, on the lane's log, so a human reviewing an arm
    // decision sees measured throughput rather than a green tick. `println!` is
    // denied workspace-wide, so this goes through `Write` explicitly.
    {
        use std::io::Write;
        let _ = writeln!(
            std::io::stdout(),
            "7deu measured: unleased {} usec / nr_throttled {} over {:?}; \
             leased {} usec / nr_throttled {} over {:?}; ratio {:.2}x",
            unleased.usage_usec,
            unleased.nr_throttled,
            unleased.wall,
            leased.usage_usec,
            leased.nr_throttled,
            leased.wall,
            leased.usage_usec as f64 / unleased.usage_usec.max(1) as f64,
        );
    }

    // ── 6. The control: an idle sibling accounts for essentially nothing ────
    let idle = measure(&mut launcher, &idle_leaf, Duration::from_millis(250));
    assert!(
        idle.usage_usec < expected_unleased / 4,
        "a leaf whose only child is asleep accounted for {} usec; the measurement is not \
         attributing CPU to the cgroup that earned it",
        idle.usage_usec
    );

    // ── Teardown of the armed leaves, BEFORE the unarmed measurement ────────
    //
    // The unarmed leaf has no quota of its own, so it takes whatever the runner
    // will give it. Leaving the lifted 4-core leaf spinning alongside would make
    // step 7 measure contention rather than the absence of a clamp.
    for (leaf, pid) in [(&mut leaf, child.pid), (&mut { idle_leaf }, idle_child.pid)] {
        launcher.kill(leaf).expect("cgroup.kill");
        let drained = (0..200).any(|_| {
            if launcher.wait_empty(leaf).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
            false
        });
        assert!(drained, "leaf {} never reached populated 0", leaf.name());
        launcher.remove(leaf).expect("remove the drained leaf");
        reap(pid);
    }

    // ── 7. An UNARMED authority never clamps (goxi launcher blocker 11) ─────
    //
    // Production ran the launcher armed while the durable `admission_handoff`
    // row was ABSENT. `evaluate_invocation_lift` maps that to `Unleased`, the
    // runner treated it as a no-op, and because the leaf had already been born
    // at 250m the "no-op" pinned every build there for its whole life: a
    // measured leaf reached 21,130,868 usec of CPU — 84x the 250,000 usec
    // escalation threshold — with `cpu.max` never leaving `25000 100000`.
    // Arming was a ~16x slowdown, strictly worse than leaving it off.
    //
    // Both halves are asserted, because either alone can pass vacuously:
    //   * the CONFIGURATION, read back from the kernel: no quota at this level;
    //   * the BEHAVIOUR, measured over a wall-clock window: it actually runs
    //     multiples faster than the clamped leaf did.
    let (unarmed_leaf, unarmed_child) = launcher
        .create_command(
            "unarmed",
            Invocation {
                id: "unarmed".to_owned(),
                fence: 2,
            },
            LeaseAuthority::Unarmed,
            &spinner_command(),
        )
        .expect("spawn a probe under an unarmed lease authority");
    let unarmed_members = read_trimmed(&root.join("unarmed").join("cgroup.procs"));
    assert!(
        unarmed_members
            .split_ascii_whitespace()
            .any(|entry| entry.parse::<i32>() == Ok(unarmed_child.pid)),
        "an unarmed authority must still CONTAIN the child: it is only the quota that is \
         omitted. Members were {unarmed_members:?}, expected pid {}",
        unarmed_child.pid
    );
    let unarmed_cpu_max = root.join("unarmed").join("cpu.max");
    assert_eq!(
        read_trimmed(&unarmed_cpu_max),
        launcher.birth_cpu_max(LeaseAuthority::Unarmed),
        "an unarmed leaf must be born at the launcher's own unrestricted line"
    );
    assert_eq!(
        read_trimmed(&unarmed_cpu_max),
        "max 100000",
        "an authority that can never grant a lift must leave the leaf with no \
         quota of its own; `25000 100000` here IS the production defect"
    );
    let unarmed = measure(&mut launcher, &unarmed_leaf, WINDOW);
    assert!(
        unarmed.usage_usec > unleased.usage_usec * 3,
        "the unarmed leaf burned {} usec against {} for the clamped leaf over comparable \
         windows. An unarmed authority must cost nothing: if these are equal, arming the \
         launcher without an armed epoch is still the ~16x regression that rolled back four \
         times.",
        unarmed.usage_usec,
        unleased.usage_usec
    );
    assert_eq!(
        unarmed.nr_throttled, 0,
        "a leaf with no quota of its own cannot be throttled at its own level; {} throttled \
         periods means it was clamped after all",
        unarmed.nr_throttled
    );
    {
        use std::io::Write;
        let _ = writeln!(
            std::io::stdout(),
            "goxi-11 measured: unarmed {} usec / nr_throttled {} over {:?}; \
             vs clamped {} usec; ratio {:.2}x (arming without an armed epoch must cost nothing)",
            unarmed.usage_usec,
            unarmed.nr_throttled,
            unarmed.wall,
            unleased.usage_usec,
            unarmed.usage_usec as f64 / unleased.usage_usec.max(1) as f64,
        );
    }
    // And the one-way lift contract holds in the other direction: nothing may
    // lower an unrestricted leaf's ceiling by "lifting" it.
    let mut unarmed_leaf = unarmed_leaf;
    assert!(
        matches!(
            launcher.fenced_lift(&mut unarmed_leaf, 2),
            Err(djinn_cgroup_launcher::Error::LiftWithoutAuthority)
        ),
        "lifting a leaf born unarmed would WRITE a quota where there was none"
    );

    // ── Teardown: cgroup-wide kill, drain, unlink ───────────────────────────
    launcher.kill(&mut unarmed_leaf).expect("cgroup.kill");
    let drained = (0..200).any(|_| {
        if launcher.wait_empty(&unarmed_leaf).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
        false
    });
    assert!(drained, "the unarmed leaf never reached populated 0");
    launcher
        .remove(&unarmed_leaf)
        .expect("remove the drained leaf");
    reap(unarmed_child.pid);
}

// ─────────────────────────────── measurement ────────────────────────────────

/// One measured window: the CPU the leaf actually accrued, and how long the
/// wall clock really ran for.
struct Measured {
    usage_usec: u64,
    nr_throttled: u64,
    wall: Duration,
}

/// Sample `cpu.stat`, sleep, sample again, and return the delta.
///
/// Deltas, not absolutes: the leaf has already been running while the previous
/// phase was measured, and the wall clock is the one actually observed rather
/// than the one requested, so a slow runner widens the expectation instead of
/// failing it.
fn measure<F, S>(
    launcher: &mut Launcher<F, S>,
    leaf: &djinn_cgroup_launcher::Leaf,
    window: Duration,
) -> Measured
where
    F: djinn_cgroup_launcher::CgroupFs,
    S: djinn_cgroup_launcher::SpawnIntoCgroup,
{
    // Real monotonic time is the point: the whole assertion is CPU consumed per
    // unit of WALL CLOCK. An injected clock would make the measurement describe
    // itself rather than the kernel, which is the failure mode this file exists
    // to eliminate. The wall interval deliberately encloses both cpu.stat
    // samples: the usage delta can include CPU consumed while either sample is
    // read, so excluding that time creates a tiny, scheduler-dependent false
    // overrun at the theoretical quota ceiling.
    #[allow(clippy::disallowed_methods)]
    let started = Instant::now();
    let before: CpuStat = launcher.sample(leaf).expect("sample cpu.stat");
    std::thread::sleep(window);
    let after: CpuStat = launcher.sample(leaf).expect("sample cpu.stat");
    let elapsed = started.elapsed();
    Measured {
        usage_usec: after.usage_usec.saturating_sub(before.usage_usec),
        nr_throttled: after.nr_throttled.saturating_sub(before.nr_throttled),
        wall: elapsed,
    }
}

/// CPU microseconds a `millicores` quota permits over `wall`, capped by how much
/// the probe command could possibly consume ([`SPINNERS`] cores).
fn quota_usec(millicores: u32, wall: Duration) -> u64 {
    let wall_usec = wall.as_micros() as u64;
    let permitted = wall_usec * u64::from(millicores) / 1000;
    permitted.min(wall_usec * SPINNERS)
}

// ──────────────────────────────── commands ──────────────────────────────────

/// [`SPINNERS`] shell busy loops. Deliberately a shell builtin loop: no binary
/// to locate, no allocation, and it saturates whatever quota it is given.
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

/// A child that exists but consumes nothing — the control for the measurement.
fn sleep_command() -> CommandSpec {
    CommandSpec {
        program: "/bin/sleep".to_owned(),
        argv: vec!["60".to_owned()],
        cwd: "/workspace".to_owned(),
        environment: vec![],
    }
}

// ───────────────────────────────── helpers ──────────────────────────────────

fn require_root() {
    let euid = unsafe { libc::geteuid() };
    assert_eq!(
        euid, 0,
        "the delegated lease proof must run as uid 0 (uid {euid} cannot mount cgroup2). It runs \
         in the `{PRIVILEGED_LANE_JOB}` CI lane; reproduce it locally with: \
         `cargo test -p djinn-cgroup-launcher --test delegated_cpu_lease_lifecycle --no-run` \
         then `docker run --rm --privileged --cgroupns=private -v \"$PWD:$PWD\" ubuntu:24.04 \
         bash -c 'mkdir -p /workspace && exec \"$0\" --ignored --test-threads 1' <the binary>`"
    );
    assert!(
        Path::new("/workspace").is_dir(),
        "/workspace is missing: the rendered CommandSpec cwd must exist before a child can exec \
         (the privileged lane creates it)"
    );
}

fn scratch_dir(label: &str) -> PathBuf {
    let base = PathBuf::from(format!("/tmp/djinn-7deu-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

/// Attempt a `cgroup2` mount and return the errno if it failed.
fn try_mount_cgroup2(target: &Path) -> Option<i32> {
    use std::os::unix::ffi::OsStrExt;
    let target = CString::new(target.as_os_str().as_bytes()).expect("path");
    let fstype = CString::new("cgroup2").expect("literal");
    let rc = unsafe {
        libc::mount(
            fstype.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    (rc != 0).then(|| std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .to_owned()
}

fn reap(pid: i32) {
    let mut status = 0;
    unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
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
        panic!(
            "no `{PRIVILEGED_LANE_JOB}` job: the only behavioural proof that the CPU lease \
             governs CPU would never execute, and an unproven lease looks identical to a \
             working one"
        )
    }) + 1;
    // Everything up to the next sibling job key (exactly two-space indent).
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
