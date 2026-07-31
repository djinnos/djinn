// Test: eprintln reports the live governor numbers AC3 requires to be RECORDED.
#![allow(clippy::print_stderr)]
//! AC3 + AC4 of `fbiy-C1`: the invocation cap bound and the live privilege
//! contract, both driven against real task-run Pods on the armed cluster.
//!
//! Sibling of `kueue_governor_conformance` (AC1/AC2), which carries the module
//! docs for the whole harness — what is production here and what is substituted,
//! the isolation rules, and the run instructions all apply unchanged. The two
//! are separate test TARGETS only because one file exceeded
//! `scripts/check-file-size.sh`; they share `tests/kueue_governor/mod.rs`.
//!
//! Run them together, single-threaded — both drive the same disposable cluster
//! and the same `pods` quota:
//!
//! ```bash
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s \
//!     --test kueue_governor_cap -- --ignored --test-threads=1
//! ```

mod kueue_governor;
use kueue_governor::*;

use djinn_k8s::launcher::{LAUNCHER_CONTAINER_NAME, TASK_RUN_CGROUP_RUNTIME_CLASS};

// ===========================================================================
// AC3 — exactly K of M contending invocations observe the lifted quota
// ===========================================================================

/// With `M` Workloads admitted and the durable cap at `K < M`, exactly `K`
/// invocations reach the lifted quota and the rest stay clamped.
///
/// `M` is the live ClusterQueue's admitted-Workload count, bounded by the
/// `pods` nominalQuota the chart installed. `K` is read back out of the durable
/// invocation-lease authority after being armed through the real operator API.
/// Neither is a literal here, and the assertion compares against the read-back
/// values.
///
/// Every unauthorized invocation still ATTEMPTS a lift, with a fence the
/// governor did not grant it. That is deliberately stronger than declining to
/// ask: it proves the kernel boundary refuses, rather than proving only that
/// this test chose not to call.
///
/// AC5-style non-vacuity, each deletion failing a different assertion:
/// * delete `grant_next`'s cap comparison → all `M` are granted → `lifted == K`
///   fails at `M`;
/// * delete the `bound_pod_uid` fence → a grant binds to the wrong Pod → the
///   bind assertion in [`authorize`] fires;
/// * delete the launcher's `leaf.invocation.fence != fence` check → the
///   unauthorized Pods lift too → `clamped == M - K` fails.
#[test]
#[ignore]
fn live_exactly_k_of_m_contending_invocations_observe_the_lifted_quota() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    ensure_probe_image(&context);
    let config = config_for(&context);
    let (clamp_millicores, leased_millicores) = rendered_quotas(&config);

    // M: launch up to the live `pods` quota, then read back what was ADMITTED.
    let quota = pods_nominal_quota(&context);
    assert!(quota >= 2, "the live pods quota is {quota}; nothing to contend over");
    let mut probes = Vec::new();
    for index in 0..quota {
        probes.push(launch_probe(
            &context,
            &config,
            0x5eed_c1a5_0001_0000 + index,
            10,
            10,
        ));
    }
    let m = admitted_workloads(&context).max(probes.len() as u64);
    eprintln!(
        "AC3 M={m} (live ClusterQueue admittedWorkloads, pods nominalQuota {quota})",
    );

    for probe in &probes {
        await_probe_record(&context, probe, "awaiting_decision");
        assert_eq!(
            leaf_cpu_max(&context, probe),
            cpu_max_line(clamp_millicores),
            "every contending invocation must start clamped",
        );
    }

    let uids: Vec<String> = probes.iter().map(|probe| probe.pod_uid.clone()).collect();
    let (k, authorized) = authorize(&uids, m);
    eprintln!("AC3 live governor: K={k} (durable authority), authorized={authorized:?}");
    assert_eq!(
        authorized.len() as i64,
        k,
        "the cap must be REACHED as well as respected: {} of a possible K={k} were authorized, \
         which is what a deleted invocation queue looks like",
        authorized.len(),
    );

    for probe in &probes {
        let granted = authorized.contains(&probe.pod_uid);
        // The unauthorized present a fence they were never granted, so the
        // refusal is the launcher's and not this test's silence.
        let presented = if granted { probe.fence } else { probe.fence ^ 1 };
        deliver_decision(&context, probe, Some(presented));
    }

    let mut lifted = Vec::new();
    let mut clamped = Vec::new();
    for probe in &probes {
        let log = await_probe_record(&context, probe, "summary");
        let observed = leaf_cpu_max(&context, probe);
        let result = probe_record(&log, "lift_attempt")
            .get("result")
            .cloned()
            .unwrap_or_default();
        eprintln!(
            "AC3 pod={} uid={} authorized={} lift={result} cpu.max={observed}",
            probe.pod,
            probe.pod_uid,
            authorized.contains(&probe.pod_uid),
        );
        if observed == cpu_max_line(leased_millicores) {
            assert_eq!(result, "accepted");
            lifted.push(probe.pod_uid.clone());
        } else {
            assert_eq!(
                observed,
                cpu_max_line(clamp_millicores),
                "an invocation that did not lift must still hold the clamp, not some third value",
            );
            assert_eq!(result, "refused");
            clamped.push(probe.pod_uid.clone());
        }
    }

    assert_eq!(
        lifted.len() as i64,
        k,
        "{} invocations observed the lifted quota against the live cap K={k}",
        lifted.len(),
    );
    assert_eq!(
        clamped.len() as u64,
        m - u64::try_from(k).expect("K fits a u64"),
        "every invocation the governor did not authorize must still be clamped",
    );
    let mut sorted_lifted = lifted.clone();
    let mut sorted_authorized = authorized.clone();
    sorted_lifted.sort();
    sorted_authorized.sort();
    assert_eq!(
        sorted_lifted, sorted_authorized,
        "the invocations that lifted must be exactly the ones the governor bound",
    );

    for probe in &probes {
        delete_job(&context, &probe.job_name);
    }
}

// ===========================================================================
// AC4 — the live container holds no cgroup-privileged capability
// ===========================================================================

/// The privilege contract, read off the RUNNING containers rather than the
/// manifest, and cross-checked against what the kernel granted the process.
///
/// The manifest half is [`guard_the_rendered_task_run_grants_no_cgroup_privileged_capability`];
/// this is the half a render cannot establish, because a RuntimeClass handler,
/// an admission webhook or a container runtime default could all add a
/// capability the manifest never mentioned. `/proc/1/status`'s `CapEff` is the
/// kernel's own answer.
#[test]
#[ignore]
fn live_the_task_run_containers_hold_no_sys_admin_or_sys_resource() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    ensure_probe_image(&context);
    let config = config_for(&context);
    let probe = launch_probe(&context, &config, 0x5eed_c1a5_0000_0004, 4, 4);
    await_probe_record(&context, &probe, "created");

    // CAP_SYS_ADMIN is bit 21, CAP_SYS_RESOURCE is bit 24.
    const SYS_ADMIN_BIT: u32 = 21;
    const SYS_RESOURCE_BIT: u32 = 24;
    for container in [LAUNCHER_CONTAINER_NAME, WORKER_CONTAINER_NAME] {
        let raw = kubectl_ok(
            &context,
            &[
                "-n",
                NAMESPACE,
                "exec",
                &probe.pod,
                "-c",
                container,
                "--",
                "sh",
                "-c",
                "grep -E '^Cap(Eff|Prm|Bnd):' /proc/self/status",
            ],
        );
        eprintln!("AC4 {container}:\n{raw}");
        for line in raw.lines() {
            let (label, hex) = line.split_once(':').expect("a /proc status line");
            let mask = u64::from_str_radix(hex.trim(), 16).expect("a capability mask is hex");
            for (name, bit) in [("SYS_ADMIN", SYS_ADMIN_BIT), ("SYS_RESOURCE", SYS_RESOURCE_BIT)] {
                assert_eq!(
                    mask & (1 << bit),
                    0,
                    "container {container} holds CAP_{name} in {label} ({hex}); the clamp is \
                     advisory for any process that can raise its own limits",
                );
            }
        }
    }

    // And the RuntimeClass contract itself, live: the class the Pod names must
    // resolve to the handler the node was taught and select on the node label.
    let class = kubectl_json(
        &context,
        &["get", "runtimeclass", TASK_RUN_CGROUP_RUNTIME_CLASS],
    );
    assert_eq!(
        class["handler"].as_str(),
        Some("runc-cgroupwritable"),
        "the installed RuntimeClass names a handler this harness never taught the node",
    );
    assert_eq!(
        class["scheduling"]["nodeSelector"]["djinn.io/cgroup-writable"].as_str(),
        Some("true"),
        "the class must keep task-run Pods off nodes that have not been conformed",
    );
    let pod = kubectl_json(&context, &["-n", NAMESPACE, "get", "pod", &probe.pod]);
    assert_eq!(
        pod["spec"]["runtimeClassName"].as_str(),
        Some(TASK_RUN_CGROUP_RUNTIME_CLASS),
        "the admitted Pod must still carry the class the renderer set",
    );
    assert_eq!(
        pod["spec"]["nodeSelector"]["djinn.io/cgroup-writable"].as_str(),
        Some("true"),
        "the RuntimeClass admission controller must have merged the class's nodeSelector into \
         the Pod; without it the Pod could land on an unconformed node",
    );

    delete_job(&context, &probe.job_name);
    let _ = probe.task_run_id;
}
