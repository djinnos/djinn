// Test: eprintln is how the RECORDED (not asserted) measurements of this file
// reach the operator reading the run, and the skip-reason channel for the gated
// half. Mirrors tests/kueue_cluster_harness.rs.
#![allow(clippy::print_stderr)]
//! The invocation governor, proved end to end on a LIVE armed-Kueue cluster
//! with a real writable-cgroup node (fbiy-C1).
//!
//! WHAT WAS MISSING, AND WHY A RENDER COULD NOT SUPPLY IT
//! -----------------------------------------------------
//! `djinn-cgroup-launcher`'s `tests/brokered_lease_lift_boundary.rs` already
//! proves the birth clamp, the fence refusal and the lift through the real
//! broker protocol on a real kernel — but in ONE process on a CI runner, on a
//! delegated cgroup root the test prepared for itself, with no Kubernetes
//! anywhere. `tests/kueue_disruption_governor.rs` proves the cap bound against
//! a real PostgreSQL governor and live Pod UIDs — but it never lifts a quota:
//! its Pods run `sleep`, and the cluster it uses has no `runc-cgroupwritable`
//! handler, no RuntimeClass and a `cgroupLauncher.mode: disabled` install.
//!
//! Between those two sits the claim nobody had measured: that a Pod which the
//! PRODUCTION renderer produced, admitted by Kueue, scheduled onto a node
//! carrying the RuntimeClass handler, running the SHIPPED launcher binary,
//! is born at the configured clamp, is actually throttled there, and moves to
//! the configured lifted quota when — and only when — the durable governor
//! authorizes it. Every step of that sentence is a place where green work has
//! landed inert in this repository this month.
//!
//! WHAT THIS FILE RUNS, AND WHAT IT SUBSTITUTES
//! --------------------------------------------
//! * The **Job** is `build_task_run_job`'s, unmodified, rendered from a config
//!   with `CgroupLauncherMode::Required` + `task_run_cgroup_writable_enabled`.
//!   The RuntimeClass name, `shareProcessNamespace`, the sidecar's command, env,
//!   capabilities and the absence of a sidecar CPU limit are all the renderer's.
//! * The **launcher** is the shipped `/opt/djinn/bin/djinn-cgroup-launcher`
//!   binary built out of this repository. Nothing about it is stubbed.
//! * The **worker container's `command`** is replaced — the single mutation this
//!   file makes to the render, and the same one
//!   `tests/kueue_disruption/mod.rs::sleep_instead_of_the_worker` makes for the
//!   same reason: the real `djinn-agent-worker` needs a coordinator, a database
//!   and a model provider, none of which exist on a disposable kind node. The
//!   replacement (`djinn-cgroup-launcher`'s `examples/governor_probe.rs`) speaks
//!   the real handshake and the real broker protocol and decides nothing.
//! * The **governor** is real: `InvocationLeaseAuthorityRepository` +
//!   `BuildLeaseRepository::grant_next` against a real PostgreSQL, driven from
//!   the live Pod UIDs, with `K` READ BACK out of the durable authority row and
//!   `M` read off the live ClusterQueue. Neither number is a constant here.
//!
//! The one seam that is neither production nor kernel is the delivery of the
//! governor's verdict into the Pod: production carries it over the coordinator
//! connection, and this harness writes it into a file with `kubectl exec`. What
//! it delivers is a fence, and the fence is then judged by the launcher, in the
//! kernel, exactly as in production — so the harness can decide WHO is asked to
//! lift and can never decide whether the lift succeeds.
//!
//! ISOLATION
//! ---------
//! Own cluster, own registry, own port, all distinct from
//! `scripts/kind/setup-kueue-cluster.sh`'s defaults (`fbiy-B0`/`B1`) and from
//! `tests/kueue_disruption*`'s (`fbiy-B2a`/`B2b`) — `down` DELETES what it is
//! given, so a shared name is a shared deletion. Kept that way by
//! [`guard_this_harness_is_disjoint_from_every_sibling_kueue_harness`].
//!
//! ```bash
//! scripts/kind/setup-kueue-cluster.sh up --cluster-name djinn-kueue-c1 \
//!     --registry-name djinn-kueue-c1-registry --registry-port 5061 \
//!     --k8s-version 1.35.0 --cgroup-writable \
//!     --values deploy/helm/djinn/tests/fixtures/kueue-governor-values.yaml
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s \
//!     --test kueue_governor_conformance -- --ignored --test-threads=1
//! scripts/kind/setup-kueue-cluster.sh down --cluster-name djinn-kueue-c1 \
//!     --registry-name djinn-kueue-c1-registry
//! ```

mod kueue_governor;
use kueue_governor::*;

use djinn_cgroup_launcher::bootstrap::RETAINED_CAPABILITY_NAMES;
use djinn_cgroup_launcher::{LeasedQuota, UnleasedQuota};
use djinn_k8s::launcher::{
    LAUNCHER_CONTAINER_NAME, LAUNCHER_UNLEASED_MILLICORES, TASK_RUN_CGROUP_RUNTIME_CLASS,
};

// ===========================================================================
// Hermetic guards — these run in the ordinary test lane, on every PR.
//
// The live half below is `#[ignore]` + env-gated and NO CI lane runs it, so
// every claim that must not regress silently lives here instead.
// ===========================================================================

/// This harness must be unable to touch any sibling harness's cluster, or the
/// developer's Tilt environment.
#[test]
fn guard_this_harness_is_disjoint_from_every_sibling_kueue_harness() {
    let accepted = run_script(
        SETUP_SCRIPT,
        &[
            "check",
            "--cluster-name",
            HARNESS_CLUSTER,
            "--registry-name",
            HARNESS_REGISTRY,
            "--registry-port",
            HARNESS_REGISTRY_PORT,
            "--k8s-version",
            HARNESS_K8S_VERSION,
            "--context",
            HARNESS_CONTEXT,
            "--cgroup-writable",
        ],
    );
    assert_eq!(
        exit_code(&accepted),
        0,
        "the script must accept this file's cluster/registry/port/context/flag; stderr: {}",
        stderr(&accepted),
    );
    assert!(
        stdout(&accepted).contains(&format!("cluster={HARNESS_CLUSTER} "))
            && stdout(&accepted).contains(&format!("context={HARNESS_CONTEXT} "))
            && stdout(&accepted).contains("cgroup-writable=true"),
        "the script must derive this file's context and understand --cgroup-writable, got: {}",
        stdout(&accepted),
    );

    for sibling in SIBLING_CLUSTERS {
        assert_ne!(
            *sibling, HARNESS_CLUSTER,
            "`down` deletes the cluster it is given, so two harnesses sharing a name destroy \
             each other's work",
        );
    }
    // And the script's own defaults, which fbiy-B0/B1 run with.
    let defaults = run_script(SETUP_SCRIPT, &["check"]);
    assert_eq!(exit_code(&defaults), 0, "stderr: {}", stderr(&defaults));
    assert!(
        !stdout(&defaults).contains(&format!("cluster={HARNESS_CLUSTER} ")),
        "this harness must not share the script's default cluster: {}",
        stdout(&defaults),
    );
    // A default `check` must ALSO report the flag off — a script that armed the
    // node unconditionally would change every sibling harness's cluster.
    assert!(
        stdout(&defaults).contains("cgroup-writable=false"),
        "--cgroup-writable must be OPT-IN; the siblings' clusters were measured without it: {}",
        stdout(&defaults),
    );
}

/// The C1 values file arms all three switches together, and the B0 file still
/// arms none of them.
///
/// The second half is the load-bearing one. `tests/kueue_cluster_harness.rs`
/// asserts `deploy/kueue/preflight.sh --mode cutover` exits 10 ("RuntimeClass
/// djinn-cgroup-writable is absent") against the B0 fixture — 6knv's AC4. That
/// assertion dies the moment anyone flips `cgroupWritable.runtimeClass.enabled`
/// there, and dies silently, because nothing in that file mentions this one.
#[test]
fn guard_the_arming_triple_is_in_the_c1_fixture_and_absent_from_the_b0_fixture() {
    let c1 = yaml_at(GOVERNOR_VALUES);
    assert_eq!(
        c1["cgroupLauncher"]["mode"].as_str(),
        Some("required"),
        "the C1 fixture must arm the launcher, or every Pod it renders runs without a sidecar",
    );
    for gate in ["runtimeClass", "taskRuns"] {
        assert_eq!(
            c1["cgroupWritable"][gate]["enabled"].as_bool(),
            Some(true),
            "cgroupWritable.{gate}.enabled must be true: the render asserts the pairing and \
             `build_task_run_job` panics on a required launcher without the RuntimeClass",
        );
    }
    assert_eq!(c1["kueue"]["armed"].as_bool(), Some(true));

    let b0 = yaml_at(B0_VALUES);
    assert_eq!(
        b0["cgroupLauncher"]["mode"].as_str(),
        Some("disabled"),
        "fbiy-C1 must not arm the B0 fixture: `preflight.sh --mode cutover` exiting 10 against \
         that cluster is 6knv's AC4",
    );
    for gate in ["runtimeClass", "taskRuns"] {
        assert_eq!(
            b0["cgroupWritable"][gate]["enabled"].as_bool(),
            Some(false),
            "fbiy-C1 must not enable cgroupWritable.{gate} in the B0 fixture",
        );
    }
}

/// `M` must be readable from the live cluster and must exceed 1, or a cap
/// strictly below it cannot exist.
#[test]
fn guard_the_c1_fixture_declares_a_pods_quota_that_admits_a_strict_cap() {
    let c1 = yaml_at(GOVERNOR_VALUES);
    let build_pods = c1["kueue"]["buildPods"]
        .as_u64()
        .expect("the C1 fixture declares kueue.buildPods");
    assert!(
        build_pods >= 2,
        "M={build_pods}: a cap K < M with K >= 1 does not exist below M=2, and AC3 would measure \
         nothing",
    );
    let chart_default = yaml_at("deploy/helm/djinn/values.yaml")["kueue"]["buildPods"].as_u64();
    assert_ne!(
        Some(build_pods),
        chart_default,
        "kueue.buildPods equals the chart default, so the live assertion would pass just as well \
         against an install that never read this fixture",
    );
}

/// The setup script must resolve the containerd namespace from the node, not
/// from a literal.
///
/// Measured 2026-07-31: the kind node runs containerd **2.2.0** under a CRI
/// configuration that declares **`version = 2`**, while the production VPS runs
/// the v3 schema. A handler block written into the wrong plugin namespace is
/// accepted by containerd and silently ignored — the RuntimeClass still
/// resolves, the Pod is still admitted, and the sandbox comes up under plain
/// `runc` with a read-only `/sys/fs/cgroup`. That failure is indistinguishable
/// from a launcher bug at the point it surfaces.
#[test]
fn guard_the_setup_script_resolves_the_containerd_schema_from_the_live_node() {
    let script =
        std::fs::read_to_string(repo_root().join(SETUP_SCRIPT)).expect("read the setup script");
    assert!(
        script.contains("containerd-config-version.sh"),
        "the script must source deploy/node/k3s/containerd-config-version.sh rather than \
         reimplementing schema detection",
    );
    assert!(
        script.contains("djinn_containerd_detect_version"),
        "the script must call the detector against the node's LIVE configuration",
    );
    assert!(
        script.contains("crictl info"),
        "appending a table is not the same as containerd loading it; the script must verify the \
         handler against what containerd PARSED",
    );
    // The two namespaces must appear nowhere as literals: both come from the
    // detector, keyed on the version it resolved.
    for namespace in [
        "io.containerd.grpc.v1.cri\".containerd.runtimes",
        "io.containerd.cri.v1.runtime'.containerd.runtimes",
    ] {
        assert!(
            !script.contains(namespace),
            "the script hardcodes the runtime table `{namespace}`; it must be derived from the \
             live schema",
        );
    }
}

/// AC4, hermetic half: the rendered task-run Pod names the RuntimeClass and
/// grants no cgroup-privileged capability to any container.
///
/// The non-vacuity is in the same test on purpose: [`privileged_capabilities`]
/// is applied to a FIXTURE container carrying `SYS_ADMIN` and to another
/// carrying `SYS_RESOURCE`, and must report both. A capability check that
/// silently matched nothing would pass the real render and this file would be
/// green while the contract was gone.
#[test]
fn guard_the_rendered_task_run_grants_no_cgroup_privileged_capability() {
    let job = render_probe_job(&armed_config(PROBE_IMAGE), "harness-project").0;
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("the renderer sets a PodSpec");
    assert_eq!(
        pod.runtime_class_name.as_deref(),
        Some(TASK_RUN_CGROUP_RUNTIME_CLASS),
        "an armed task-run Pod must name the writable-cgroup RuntimeClass",
    );

    let launcher = pod
        .init_containers
        .as_ref()
        .and_then(|containers| {
            containers
                .iter()
                .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        })
        .expect("the armed renderer emits the cgroup-launcher sidecar");
    let worker = pod
        .containers
        .iter()
        .find(|container| container.name == WORKER_CONTAINER_NAME)
        .expect("the renderer emits the worker container");

    for container in [launcher, worker] {
        assert!(
            privileged_capabilities(container).is_empty(),
            "container {} grants {:?}; the launcher establishes nothing by mount and the worker \
             establishes nothing at all",
            container.name,
            privileged_capabilities(container),
        );
    }
    // The sidecar's positive contract, so "no privileged capability" cannot be
    // satisfied by a render that grants none at all.
    let granted: Vec<String> = launcher
        .security_context
        .as_ref()
        .and_then(|context| context.capabilities.as_ref())
        .and_then(|capabilities| capabilities.add.clone())
        .unwrap_or_default();
    assert_eq!(
        granted,
        RETAINED_CAPABILITY_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
        "the sidecar must hold exactly the launcher crate's retained set",
    );

    // Non-vacuity: the same predicate, against a container that HAS the
    // capability.
    for forbidden in ["SYS_ADMIN", "SYS_RESOURCE"] {
        let mut mutated = launcher.clone();
        let capabilities = mutated
            .security_context
            .as_mut()
            .and_then(|context| context.capabilities.as_mut())
            .expect("the sidecar declares capabilities");
        capabilities
            .add
            .get_or_insert_with(Vec::new)
            .push(forbidden.to_owned());
        assert_eq!(
            privileged_capabilities(&mutated),
            vec![forbidden.to_owned()],
            "the capability assertion must FIRE on a fixture build that adds {forbidden}",
        );
    }
}

/// The lifted quota this harness measures against must be the one the renderer
/// derived from the Pod's own CPU limit, and it must actually be a lift.
#[test]
fn guard_the_rendered_quotas_are_a_real_multiplication() {
    let (_, rendered) = rendered_quotas(&armed_config(PROBE_IMAGE));
    assert_eq!(
        u32::from(LAUNCHER_UNLEASED_MILLICORES),
        u32::from(UnleasedQuota::DEFAULT_MILLICORES),
        "the render's clamp must be the launcher crate's own default",
    );
    assert!(
        rendered >= LeasedQuota::MIN_MILLICORES,
        "a leased quota below {} is not a lift at all, and the launcher would refuse the config",
        LeasedQuota::MIN_MILLICORES,
    );
    let ratio = u64::from(rendered) * 1_000 / u64::from(LAUNCHER_UNLEASED_MILLICORES);
    assert!(
        ratio >= REQUIRED_THROUGHPUT_MULTIPLE_MILLI,
        "the rendered quotas differ by {ratio}/1000x, below the {REQUIRED_THROUGHPUT_MULTIPLE_MILLI}/1000x \
         this file asserts on measured throughput; the assertion could never pass",
    );
}

/// The throughput assertion must fail on a PINNED quota.
///
/// This is AC1's non-vacuity, in the lane that always runs: the live half pins
/// the quota by hand (see the PR body), and this keeps the arithmetic that
/// judges it honest in the meantime. A helper that returned "multiplied" for an
/// unchanged rate would make the whole live measurement decorative.
#[test]
fn guard_an_unchanged_throughput_is_not_a_multiplication() {
    assert!(throughput_multiple_milli(12_000, 96_000) >= REQUIRED_THROUGHPUT_MULTIPLE_MILLI);
    // Pinned: same rate before and after.
    assert!(throughput_multiple_milli(12_000, 12_000) < REQUIRED_THROUGHPUT_MULTIPLE_MILLI);
    // Slightly better, but not a lift.
    assert!(throughput_multiple_milli(12_000, 30_000) < REQUIRED_THROUGHPUT_MULTIPLE_MILLI);
    // A clamp phase that produced nothing must not read as an infinite gain.
    assert_eq!(throughput_multiple_milli(0, 96_000), 0);
}

// ===========================================================================
// AC1 — born clamped, throttled there, lifted, and 3x faster for it
// ===========================================================================

/// A Pod the production renderer produced runs under the writable-cgroup
/// RuntimeClass, is born at the configured clamp, ACCUMULATES throttling at it,
/// transitions to the configured lifted quota on an authorized fence, and
/// completes at least 3x the work per second afterwards.
///
/// Every number is read from somewhere that could disagree with the code:
///
/// * the clamp and the lifted quota come off the RENDERED sidecar env;
/// * `cpu.max` is read out of the launcher container's own `/sys/fs/cgroup`,
///   i.e. from the kernel, at three points — before the attempt, after it, and
///   at the end;
/// * `throttled_usec` comes from `cpu.stat` through the broker's own `sample`
///   control, and is asserted to ADVANCE, not merely to be non-zero: a
///   `cpu.max` write with no effect leaves it at 0, and a leaf that was never
///   scheduled leaves it at 0 too;
/// * throughput is completed `sha256sum` digests per second, counted from the
///   child's own output.
///
/// Non-vacuity: pin the quota (render the leased millicores equal to the clamp)
/// and the 3x assertion fails while everything else still passes — the
/// transition, the fence and the RuntimeClass are all unchanged by it. The
/// arithmetic that judges it is kept honest in the always-on lane by
/// [`guard_an_unchanged_throughput_is_not_a_multiplication`].
#[test]
#[ignore]
fn live_an_authorized_invocation_is_clamped_throttled_and_lifted_to_three_times_the_throughput() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    ensure_probe_image(&context);
    let config = config_for(&context);
    let (clamp_millicores, leased_millicores) = rendered_quotas(&config);

    let probe = launch_probe(&context, &config, 0x5eed_c1a5_0000_0001, 12, 12);
    eprintln!(
        "AC1 pod={} uid={} invocation={}",
        probe.pod, probe.pod_uid, probe.invocation
    );

    // The clamp, from the kernel, before anything is asked of the governor.
    let log = await_probe_record(&context, &probe, "awaiting_decision");
    let born = leaf_cpu_max(&context, &probe);
    assert_eq!(
        born,
        cpu_max_line(clamp_millicores),
        "the leaf must be born at the RENDERED clamp of {clamp_millicores}m",
    );
    let clamp_throttle_advances = phase_number(&log, "clamp", "throttle_advances");
    let clamp_throttled = phase_number(&log, "clamp", "throttled_usec");
    eprintln!(
        "AC1 clamp: cpu.max={born} throttled_usec+={clamp_throttled} advances={clamp_throttle_advances} \
         units={} rate_milli={}",
        phase_number(&log, "clamp", "units"),
        phase_number(&log, "clamp", "rate_milli"),
    );
    assert!(
        clamp_throttle_advances >= 2,
        "cpu.stat throttled_usec advanced {clamp_throttle_advances} time(s) at the clamp; a \
         quota that is written but not enforced leaves it flat, and that is exactly the \
         production defect a cpu.max read-back cannot see",
    );
    assert!(
        clamp_throttled > 0,
        "no throttling accumulated at the clamp",
    );

    // The governor decides. One Pod, so the cap is exercised in the AC3 test;
    // here the point is that the fence the probe presents came from a real
    // grant bound to this Pod's live UID.
    let (_, authorized) = authorize(std::slice::from_ref(&probe.pod_uid), 2);
    assert_eq!(
        authorized,
        vec![probe.pod_uid.clone()],
        "the governor must authorize this Pod, or there is no lift to measure",
    );
    deliver_decision(&context, &probe, Some(probe.fence));

    let log = await_probe_record(&context, &probe, "summary");
    let lifted = leaf_cpu_max(&context, &probe);
    assert_eq!(
        probe_record(&log, "lift_attempt")
            .get("result")
            .map(String::as_str),
        Some("accepted"),
        "the broker refused an authorized lift:\n{log}",
    );
    assert_eq!(
        lifted,
        cpu_max_line(leased_millicores),
        "the leaf must transition to the RENDERED leased quota of {leased_millicores}m",
    );

    let clamp_rate = phase_number(&log, "clamp", "rate_milli");
    let post_rate = phase_number(&log, "post", "rate_milli");
    let multiple = throughput_multiple_milli(clamp_rate, post_rate);
    eprintln!(
        "AC1 measured: cpu.max {born} -> {lifted}; digests/s {}/1000 -> {}/1000 = {multiple}/1000x",
        clamp_rate, post_rate,
    );
    assert!(
        multiple >= REQUIRED_THROUGHPUT_MULTIPLE_MILLI,
        "the lift multiplied real throughput by only {multiple}/1000x ({clamp_rate}/1000 -> \
         {post_rate}/1000 digests per second); the quota moved in the kernel but the work did \
         not, which is what an ancestor clamp looks like",
    );

    delete_job(&context, &probe.job_name);
}

// ===========================================================================
// AC2 — a wrong fence changes nothing
// ===========================================================================

/// A lift presenting a fence the invocation was not begun with is refused, and
/// the clamp is BYTE-IDENTICAL before and after the attempt.
///
/// Non-vacuity: deliver the correct fence instead and the post-attempt read
/// becomes the leased quota, so both assertions fail. That mutation is one
/// argument away and is reported in the PR body.
#[test]
#[ignore]
fn live_a_wrong_fence_lift_leaves_the_clamp_unchanged() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    ensure_probe_image(&context);
    let config = config_for(&context);
    let (clamp_millicores, leased_millicores) = rendered_quotas(&config);
    assert_ne!(
        clamp_millicores, leased_millicores,
        "with the two quotas equal, an accepted lift would be indistinguishable from a refused one",
    );

    let fence = 0x5eed_c1a5_0000_0002;
    let probe = launch_probe(&context, &config, fence, 8, 8);
    await_probe_record(&context, &probe, "awaiting_decision");

    let before = leaf_cpu_max(&context, &probe);
    assert_eq!(before, cpu_max_line(clamp_millicores));
    // One bit off. Not a random value: a fence that differs minimally proves the
    // comparison is an equality and not a range or a truthiness test.
    deliver_decision(&context, &probe, Some(fence ^ 1));
    let log = await_probe_record(&context, &probe, "lift_attempt");
    let after = leaf_cpu_max(&context, &probe);

    let attempt = probe_record(&log, "lift_attempt");
    eprintln!(
        "AC2 fence {fence} -> presented {}: {:?}; cpu.max {before} -> {after}",
        fence ^ 1,
        attempt,
    );
    assert_eq!(
        attempt.get("result").map(String::as_str),
        Some("refused"),
        "the broker accepted a lift carrying the wrong fence:\n{log}",
    );
    // `Launcher::fenced_lift` returns `Error::FenceMismatch`; the broker maps it
    // onto the wire as the LEGIBLE `ControlRejection::Fence` rather than the
    // indistinguishable `InvalidControl` — which is the distinction goxi
    // launcher blocker 14 was raised for, because "the broker refused the
    // control" with no reason is what production saw for weeks. Asserting the
    // wire form is asserting what the worker actually receives.
    assert_eq!(
        attempt.get("error").map(String::as_str),
        Some("ControlRejected(Fence)"),
        "the refusal must be the FENCE and must say so; an `InvalidControl` here would be the \
         opaque refusal that hid the production defect: {attempt:?}",
    );
    assert_eq!(
        after, before,
        "the clamp moved across a refused lift: {before} -> {after}",
    );

    // And it stays refused: the leaf is still clamped at the end of the run,
    // so nothing lifted it late.
    let log = await_probe_record(&context, &probe, "summary");
    assert_eq!(
        leaf_cpu_max(&context, &probe),
        before,
        "the clamp moved after the refusal:\n{log}",
    );
    delete_job(&context, &probe.job_name);
}
