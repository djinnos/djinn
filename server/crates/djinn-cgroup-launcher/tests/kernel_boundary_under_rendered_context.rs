//! Adversarial proof of the launcher kernel boundary under the rendered Pod
//! security context (djinn board task zf13, proposal goxi AC2).
//!
//! 7os5 proved the BROKER-facing half of the boundary with fake kernel seams:
//! wrong peer PID/UID, child and sibling connections, forged own-or-sibling
//! controls, nonce replay and control-descriptor closure. This file proves the
//! KERNEL-facing half that no fake seam can establish — a real UID-1001 child,
//! born by `clone3(CLONE_INTO_CGROUP)` into a real invocation cgroup and taken
//! through the production `close_range` + `child::prepare_child` boundary,
//! attacking a real, live, non-dumpable UID-1000 worker and being denied on
//! every path, with the worker healthy, both invocation quotas untouched and no
//! unauthorized cgroup created. Positive controls in the same suite prove the
//! legitimate worker connection still creates its assigned invocation and that
//! only a matching durable fencing token lifts quota, so the suite cannot pass
//! by denying everything.
//!
//! ## Where these proofs run
//!
//! They need uid 0, real UID separation, a shared PID namespace, cgroup-v2
//! delegation and an installable seccomp filter, so they are `#[ignore]`d for
//! ordinary unprivileged runs and executed by the dedicated privileged CI lane
//! (`launcher-kernel-boundary` in `.github/workflows/quality-gate.yml`).
//!
//! ## Why they cannot silently skip
//!
//! Two mechanisms, both always-on:
//!
//! 1. Inside each proof, `require_privileged_environment` PANICS with the
//!    concrete missing capability. There is no environment probe that returns
//!    "not applicable" — a degraded host fails the test.
//! 2. [`the_privileged_lane_is_wired_and_this_proof_cannot_silently_skip`] is
//!    NOT ignored, so it runs in every ordinary shard. It asserts the lane
//!    exists, runs this exact binary with `--ignored`, is not
//!    `continue-on-error`, and declares an expected proof count equal to the
//!    number of `#[ignore]`d proofs in this file — so adding a proof without
//!    wiring it, or a lane that executes zero tests, is a red build.

mod rendered_boundary;

use std::path::Path;

use djinn_cgroup_launcher::broker::{WORKER_GID, WORKER_UID};
use djinn_cgroup_launcher::child::{ARTIFACT_GID, CHILD_UID};
use djinn_cgroup_launcher::{
    ChildProcess, CommandSpec, Error, Invocation, Launcher, LauncherConfig, NativeCgroupFs,
    SpawnIntoCgroup, UnleasedQuota,
};

use rendered_boundary::{
    Attempt, LIFTED_CPU_MAX, RenderedContext, UNLEASED_CPU_MAX, Worker, chown, fixture_path, pipe,
    read_report, repo_root, require_privileged_environment, run_as, serve_broker, set_mode,
    unique_slot,
};

/// The workflow job that must execute the `#[ignore]`d proofs below.
const PRIVILEGED_LANE_JOB: &str = "launcher-kernel-boundary";
/// Marker the lane uses to declare how many proofs it expects to execute.
const EXPECTED_PROOFS_KEY: &str = "ZF13_EXPECTED_PROOFS";

// ═══════════════════ always-on: the lane cannot silently skip ════════════════

#[test]
fn the_privileged_lane_is_wired_and_this_proof_cannot_silently_skip() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/kernel_boundary_under_rendered_context.rs"),
    )
    .expect("read this test file to count its privileged proofs");
    // Only attributes at the start of a line; prose mentions are backticked.
    let declared = source.matches("\n#[ignore").count();
    assert!(
        declared > 0,
        "this file must declare privileged proofs; an empty suite proves nothing"
    );

    let lane = privileged_lane_block();
    assert!(
        lane.contains("--test kernel_boundary_under_rendered_context"),
        "the privileged lane must run THIS test binary"
    );
    assert!(
        lane.contains("--ignored --test-threads 1"),
        "the privileged lane must run the `#[ignore]`d proofs serially, not skip past them"
    );
    assert!(
        !lane.contains("continue-on-error"),
        "the privileged lane may not swallow its own failure; a security proof least of all"
    );
    assert!(
        lane.contains(&format!("{EXPECTED_PROOFS_KEY}: \"{declared}\"")),
        "the lane must declare `{EXPECTED_PROOFS_KEY}: \"{declared}\"` so a run that \
         executes fewer than the {declared} declared proofs fails instead of passing"
    );

    // The fixture the proofs drive their identities from must agree with the
    // constants this crate enforces at runtime.
    let context = RenderedContext::load();
    assert_eq!(context.u32("worker_run_as_user"), WORKER_UID);
    assert_eq!(context.u32("worker_run_as_group"), WORKER_GID);
    assert_eq!(context.u32("child_run_as_user"), CHILD_UID);
    assert_eq!(context.u32("child_run_as_group"), ARTIFACT_GID);
    assert_eq!(context.u32("pod_fs_group"), ARTIFACT_GID);
    assert_eq!(context.get("child_umask"), "0002");
    assert_eq!(
        context.get("unleased_millicores"),
        &UnleasedQuota::DEFAULT_MILLICORES.to_string()
    );
    assert_ne!(
        context.u32("worker_run_as_user"),
        context.u32("child_run_as_user"),
        "the worker and its launched child must never share a uid"
    );
    assert!(
        fixture_path().is_file(),
        "the rendered security-context fixture must ship with the suite"
    );
}

// ══════════════════════ privileged proof 1: the attacks ══════════════════════

/// A real UID-1001 launched child attacks a real UID-1000 worker and is denied
/// on every path goxi AC2 names, while the worker stays healthy, both
/// invocation quotas are unchanged and no unauthorized cgroup is created.
/// Positive controls run on the same live topology.
#[ignore = "privileged: needs uid 0, real UID separation, cgroup-v2 delegation and seccomp \
            (CI job launcher-kernel-boundary)"]
#[test]
fn a_uid1001_launched_child_is_denied_on_every_kernel_boundary_path() {
    let context = RenderedContext::load();
    let environment = require_privileged_environment(&context);

    // 1. A live, non-dumpable UID-1000 worker, forked while single-threaded.
    let mut worker = Worker::fork(&environment, &context);
    let (report_read, report_write) = pipe();

    // 2. The privileged broker over the real Unix transport, and the worker's
    //    authenticated connection through it (SO_PEERCRED, private credential).
    let _server = serve_broker(&environment, &context, worker.pid, report_write);
    worker.send("connect");
    assert_eq!(
        worker.expect_line(),
        "connected",
        "POSITIVE CONTROL: the legitimate UID-1000 worker must authenticate"
    );

    // 3. POSITIVE CONTROL: the worker creates its assigned invocations. The
    //    attack invocation's child is the adversary; the others really exec.
    assert_eq!(worker.expect_line(), "created");
    let mut leaves = environment.cgroup_children();
    leaves.sort();
    assert_eq!(
        leaves,
        ["attack", "execed", "legit"],
        "exactly the three authorized invocation cgroups exist"
    );
    for leaf in ["legit", "attack"] {
        assert_eq!(
            environment.quota(leaf),
            UNLEASED_CPU_MAX,
            "{leaf} must start at the unleased broker quota"
        );
    }

    // 4. The exec'd UID-1001 adversary's own findings, proving the credential
    //    drop and seccomp filter survive `execve`.
    let execed: Vec<String> = worker.collect_until("execed-done");
    let execed: Vec<&str> = execed
        .iter()
        .filter_map(|line| line.strip_prefix("execed "))
        .collect();
    assert!(
        !execed.is_empty(),
        "the exec'd adversary produced no output; the boundary was not exercised"
    );
    for probe in ["environ", "maps", "fd", "root", "cwd", "signal", "socket"] {
        assert!(
            execed.contains(&format!("{probe}=denied").as_str()),
            "post-exec adversary reached {probe} on the worker: {execed:?}"
        );
    }
    assert!(
        execed.contains(&format!("uid={CHILD_UID}").as_str()),
        "the exec'd child must run as the rendered child uid: {execed:?}"
    );
    assert!(
        execed.contains(&format!("gid={ARTIFACT_GID}").as_str()),
        "the exec'd child must run in the artifact group: {execed:?}"
    );

    // 5. The in-cgroup adversary's syscall-level report.
    let attempts = read_report(report_read);
    assert_eq!(
        value(&attempts, "identity_uid").rc,
        i64::from(CHILD_UID),
        "the launched child must be the rendered child uid"
    );
    assert_eq!(value(&attempts, "identity_gid").rc, i64::from(ARTIFACT_GID));
    assert_eq!(
        value(&attempts, "identity_groups").rc,
        0,
        "the child must hold no supplementary groups"
    );
    assert_eq!(
        value(&attempts, "identity_umask").rc,
        0o2,
        "the child must run under the rendered umask 0002"
    );

    // Every attack path goxi AC2 names, with the errno that denied it.
    for (probe, allowed) in [
        // /proc/<worker-pid>/{root,fd,mem} and the rest of the procfs surface.
        ("proc_mem", &[libc::EACCES, libc::EPERM][..]),
        ("proc_environ", &[libc::EACCES, libc::EPERM]),
        ("proc_maps", &[libc::EACCES, libc::EPERM]),
        ("proc_root", &[libc::EACCES, libc::EPERM]),
        ("proc_fd", &[libc::EACCES, libc::EPERM]),
        ("proc_fd_link", &[libc::EACCES, libc::EPERM, libc::ENOENT]),
        // ptrace attach/seize.
        ("ptrace_attach", &[libc::EPERM]),
        ("ptrace_seize", &[libc::EPERM]),
        // Cross-process memory read and write.
        ("process_vm_readv", &[libc::EPERM]),
        ("process_vm_writev", &[libc::EPERM]),
        // Signals, stop and injection.
        ("signal_stop", &[libc::EPERM]),
        ("signal_kill", &[libc::EPERM]),
        ("signal_usr1", &[libc::EPERM]),
        // Control-descriptor enumeration and reuse.
        ("control_fd_reuse", &[libc::EBADF]),
        ("pidfd_getfd", &[libc::EPERM, libc::EACCES, libc::ESRCH]),
        // Broker credential recovery and a broker connection as uid 1001.
        ("broker_credential_read", &[libc::EACCES, libc::EPERM]),
        ("broker_socket_connect", &[libc::EACCES, libc::EPERM]),
        // Unauthorized cgroup creation and quota tampering.
        ("cgroup_create", &[libc::EACCES, libc::EPERM]),
        ("cgroup_quota_write", &[libc::EACCES, libc::EPERM]),
        // Re-privileging.
        ("regain_uid", &[libc::EPERM]),
        ("regain_caps", &[libc::EPERM]),
        ("unshare_userns", &[libc::EPERM]),
        ("mount_any", &[libc::EPERM]),
    ] {
        let attempt = value(&attempts, probe);
        assert!(
            attempt.rc < 0,
            "{probe} SUCCEEDED from the UID-1001 child (rc {}); the kernel boundary is open",
            attempt.rc
        );
        assert!(
            allowed.contains(&attempt.errno),
            "{probe} was denied with errno {} but the boundary must deny it with one of \
             {allowed:?}",
            attempt.errno
        );
    }

    // 6. Invariants: worker healthy, quotas untouched, no new cgroup.
    assert!(
        worker.is_alive(),
        "the worker must survive every attack; it was killed or stopped"
    );
    for leaf in ["legit", "attack"] {
        assert_eq!(
            environment.quota(leaf),
            UNLEASED_CPU_MAX,
            "{leaf}'s invocation quota changed under attack"
        );
    }
    let mut after = environment.cgroup_children();
    after.sort();
    assert_eq!(
        after,
        ["attack", "execed", "legit"],
        "an unauthorized cgroup was created under the delegated root"
    );

    // 7. POSITIVE CONTROL: only a matching durable fencing token lifts quota.
    worker.send("controls");
    assert_eq!(
        worker.expect_line(),
        "mismatched-fence-rejected true",
        "a mismatched fencing token must not lift the quota"
    );
    assert_eq!(
        environment.quota("legit"),
        UNLEASED_CPU_MAX,
        "a rejected fence must leave the unleased quota in place"
    );
    worker.send("lift");
    assert_eq!(
        worker.expect_line(),
        "matching-fence-lifted true",
        "the matching durable fencing token must lift the quota"
    );
    assert_eq!(worker.expect_line(), "controls-done");
    assert_eq!(
        environment.quota("legit"),
        LIFTED_CPU_MAX,
        "the matching fence must actually write the lifted cpu.max"
    );
    assert!(
        worker.is_alive(),
        "the worker must still be live after the positive controls"
    );

    // The still-unleased sibling proves the lift was invocation-scoped.
    assert_eq!(environment.quota("attack"), UNLEASED_CPU_MAX);
}

// ════════════════ privileged proof 2: artifact ownership (7os5) ══════════════

/// The setgid GID-1000 + umask-0002 fixture 7os5 could only describe. It now
/// EXECUTES: two real processes at the distinct rendered worker/child UIDs,
/// both in the artifact group, mutually edit each other's setgid artifacts.
#[ignore = "privileged: needs uid 0 to become both uid 1000 and uid 1001 \
            (CI job launcher-kernel-boundary)"]
#[test]
fn setgid_artifacts_stay_mutually_editable_across_the_distinct_worker_and_child_uids() {
    use std::os::unix::fs::MetadataExt;

    let context = RenderedContext::load();
    // Deliberately reuses the same fail-loud precondition gate: this proof was
    // unproven precisely because it was allowed to skip.
    let _environment = require_privileged_environment(&context);

    let worker_uid = context.u32("worker_run_as_user");
    let child_uid = context.u32("child_run_as_user");
    let artifact_gid = context.u32("child_run_as_group");
    assert_ne!(worker_uid, child_uid, "distinct identities are the point");
    assert_eq!(artifact_gid, context.u32("pod_fs_group"));

    let base = std::path::PathBuf::from(format!("/tmp/djinn-zf13-artifacts-{}", unique_slot()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir(&base).expect("create artifact base");
    set_mode(&base, 0o2775); // rwxrwsr-x: setgid, group-writable.
    chown(&base, worker_uid, artifact_gid);

    let create = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
    let append = libc::O_WRONLY | libc::O_APPEND;

    // Worker creates, child edits.
    let worker_owned = base.join("worker-owned.txt");
    assert_eq!(run_as(&worker_owned, worker_uid, artifact_gid, create), 0);
    assert_eq!(
        run_as(&worker_owned, child_uid, artifact_gid, append),
        0,
        "the UID-1001 child must be able to edit the worker's artifact"
    );

    // Child creates, worker edits.
    let child_owned = base.join("child-owned.txt");
    assert_eq!(run_as(&child_owned, child_uid, artifact_gid, create), 0);
    assert_eq!(
        run_as(&child_owned, worker_uid, artifact_gid, append),
        0,
        "the UID-1000 worker must be able to edit the child's artifact"
    );

    for (path, owner) in [(&worker_owned, worker_uid), (&child_owned, child_uid)] {
        let metadata = std::fs::metadata(path).expect("artifact metadata");
        assert_eq!(
            metadata.uid(),
            owner,
            "each artifact keeps its creator's uid"
        );
        assert_eq!(
            metadata.gid(),
            artifact_gid,
            "the setgid directory must propagate the artifact group"
        );
        assert_ne!(
            metadata.mode() & 0o020,
            0,
            "umask 0002 must leave artifacts group-writable"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

// ═════════ privileged proof 3: incompatible ownership rejects the spawn ══════

/// An incompatible volume/cgroup-ownership mode is refused at readiness, before
/// any leaf is created and before any child can exec.
#[ignore = "privileged: needs uid 0 to own and re-own a delegated cgroup root \
            (CI job launcher-kernel-boundary)"]
#[test]
fn an_incompatible_volume_ownership_mode_rejects_the_spawn_before_exec() {
    let context = RenderedContext::load();
    let environment = require_privileged_environment(&context);

    /// A clone seam that must never be reached.
    struct NeverClone;
    impl SpawnIntoCgroup for NeverClone {
        fn spawn_into_cgroup(
            &mut self,
            _: std::os::fd::RawFd,
            _: &Invocation,
            _: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            panic!("an incompatible ownership mode must never reach the clone seam");
        }
    }

    let expected_uid = context.u32("launcher_expected_uid");
    let foreign_uid = context.u32("worker_run_as_user");

    // 1. Opening the delegated root under an incompatible ownership expectation
    //    is refused outright — the launcher never even gets a filesystem seam.
    let error = NativeCgroupFs::open(&environment.root, foreign_uid)
        .err()
        .expect("a delegated root owned by another uid must be rejected");
    assert!(
        matches!(error, Error::IncompatibleOwnership { .. }),
        "expected an ownership rejection, got {error:?}"
    );

    // 2. Even holding an opened root, a launcher configured for a different
    //    owner fails at construction, so the clone seam is never reached.
    let fs = NativeCgroupFs::open(&environment.root, expected_uid)
        .expect("the correctly owned delegated root must be accepted");
    let error = Launcher::new(
        fs,
        NeverClone,
        LauncherConfig::new(None, None, foreign_uid).expect("launcher config"),
    )
    .err()
    .expect("an incompatible ownership mode must reject the launcher");
    assert!(
        matches!(
            error,
            Error::IncompatibleOwnership {
                expected,
                actual
            } if expected == foreign_uid && actual == expected_uid
        ),
        "expected an ownership rejection naming both uids, got {error:?}"
    );
    assert!(
        environment.cgroup_children().is_empty(),
        "no invocation cgroup may be created once ownership is incompatible"
    );

    // 3. Re-owning the root away from the launcher reproduces the same refusal
    //    on the shipped path, confirming the check is the ownership check.
    chown(&environment.root, foreign_uid, 0);
    assert!(matches!(
        NativeCgroupFs::open(&environment.root, expected_uid),
        Err(Error::IncompatibleOwnership { .. })
    ));
    chown(&environment.root, expected_uid, 0);
}

/// The privileged lane's own YAML block, isolated from sibling jobs so this
/// guard asserts the lane's wiring and never trips over unrelated CI edits.
fn privileged_lane_block() -> String {
    let workflow = std::fs::read_to_string(repo_root().join(".github/workflows/quality-gate.yml"))
        .expect("read the CI workflow that must host the privileged lane");
    let header = format!("\n  {PRIVILEGED_LANE_JOB}:\n");
    let start = workflow.find(&header).unwrap_or_else(|| {
        panic!(
            "no `{PRIVILEGED_LANE_JOB}` job: goxi AC2 would have no executing proof, and an \
             unproven kernel boundary looks identical to a passing one"
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

fn value(attempts: &std::collections::BTreeMap<String, Attempt>, key: &str) -> Attempt {
    *attempts
        .get(key)
        .unwrap_or_else(|| panic!("the adversary did not report `{key}`: {attempts:?}"))
}
