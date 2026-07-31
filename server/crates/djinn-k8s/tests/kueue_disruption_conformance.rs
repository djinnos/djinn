// Test: eprintln is how the RECORDED (not asserted) measurements of this file
// reach the operator reading the run, and the skip-reason channel for the gated
// half. Mirrors tests/kueue_cluster_harness.rs.
#![allow(clippy::print_stderr)]
//! Disruption containment on the LIVE armed-Kueue cluster (fbiy-B2a / `c6ej`).
//!
//! `fbiy-A1` (#2833) built the containment — one immutable `metadata.uid` bound
//! on first observation and never re-bound, a Pod-absent-plus-Job-nonterminal
//! arm that resolves the watch and foreground-deletes the Job, a replacement Pod
//! never adopted, a cleanly `Complete` Job excluded — and proved all of it
//! against an in-process `FakeCluster`, saying in its own module docs that the
//! fake "does not substitute for the live-cluster proof, which is `fbiy-B2`'s
//! job". This is that proof. It did not confirm the fake.
//!
//! FOUR LIVE FINDINGS (measured 2026-07-30, Kubernetes 1.31 / Kueue 0.19.0)
//! -----------------------------------------------------------------------
//! Full narrative in the PR body; what each one costs this file is inline at
//! the code it constrains.
//!
//! 1. **The chart's armed topology admitted nothing.** Its ClusterQueue rendered
//!    with no `namespaceSelector` (Kueue reads `null` as *matches no namespace*)
//!    and covered `["pods"]` alone while every rendered build Job requests `cpu`
//!    and `memory`. Either alone wedges the install: captured, suspended, never
//!    admitted. Fixed in the chart by `fbiy-B1` (#2841);
//!    [`repair_cluster_queue_for_admission`] is the disposable-cluster repair
//!    that let this file measure anything at all, and is a NO-OP against the
//!    fixed chart — it reports whether it was needed, which is `01ze`'s AC4.
//! 2. **A force-deleted Pod mints NO replacement.** The renderer sets
//!    `backoffLimit: 0`, so the first Pod loss is an immediate Job failure:
//!    `FailureTarget` → `Failed` in ~5s, Workload `Finished`, `pods` usage to 0,
//!    no second Pod ever. So the Job is already `Failed` when any 15s poll sees
//!    the missing Pod, and `watch_infra_death` resolves through the PRE-EXISTING
//!    `job_failed_reason` arm — **A1's containment arm is never reached this
//!    way**, which contradicts this task's AC1/AC2 premise.
//! 3. **A1's arm IS reachable — through a Kueue EVICTION.** Eviction
//!    (`stopPolicy: HoldAndDrain`; preemption and a quota cut share the code
//!    path) re-suspends the Job and deletes its Pod, leaving `suspend: true`,
//!    no `Failed` condition, Workload `Evicted` but not `Finished` — exactly
//!    `job_failed_reason() == None && !job_completed_cleanly()`. Releasing the
//!    queue re-admits the same Workload with a NEW Pod UID, so the eviction is
//!    recoverable, the Job is owed a Pod, and the containment deletes it. That
//!    is the live subject AC2 needed.
//! 4. **`kube::Client` panics on construction here.** `workspace-hack` unifies
//!    rustls with both `ring` and `aws-lc-rs`, and it panics for `http://` as
//!    readily as `https://`, so there is no TLS-free route around it.
//!    `tests/kueue_cluster_harness.rs` drove `kubectl` instead; that is not
//!    available here, because proving "the coordinator never adopts the
//!    replacement" means calling the coordinator's own watch. See
//!    [`install_crypto_provider`].
//!
//! GATING, ISOLATION, AND WHAT IS NOT HERE
//! ---------------------------------------
//! Same split as `tests/kueue_cluster_harness.rs`: the `live_*` tests are
//! `#[ignore]` + `DJINN_TEST_KUEUE_CLUSTER=1` and NO CI lane runs them, so every
//! assertion that must not regress silently is a `guard_*` test with no
//! `#[ignore]`, running in the ordinary `cargo test -p djinn-k8s` lane.
//!
//! This file owns its own cluster, registry and port, all distinct from the
//! script's defaults, because `fbiy-B1` runs the same script concurrently and
//! `down` DELETES what it is given. Kept that way by
//! [`guard_this_harness_is_disjoint_from_the_b0_defaults_and_the_tilt_cluster`].
//!
//! `c6ej`'s AC4 (coordinator-restart interleavings) is deliberately split out as
//! `fbiy-B2b`: killing a coordinator between Job creation and Workload admission
//! requires a coordinator, and this harness has none by design (Postgres
//! disabled, djinn-server in `ImagePullBackOff`).
//!
//! ```bash
//! scripts/kind/setup-kueue-cluster.sh up --cluster-name djinn-kueue-b2 \
//!     --registry-name djinn-kueue-b2-registry --registry-port 5053
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s \
//!     --test kueue_disruption_conformance -- --ignored --test-threads=1
//! scripts/kind/setup-kueue-cluster.sh down --cluster-name djinn-kueue-b2 \
//!     --registry-name djinn-kueue-b2-registry
//! ```

use std::collections::BTreeSet;
use std::process::Command;
use std::time::Duration;

use djinn_core::clock::{Clock, SystemClock};
use djinn_db::{InvocationLeaseAuthorityRow, InvocationLeaseMode};
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_runtime::{RunHandle, SessionRuntime};
use djinn_supervisor::ConnectionRegistry;
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};

mod kueue_disruption;
use kueue_disruption::*;

// ===========================================================================
// Hermetic guards — these run in the ordinary test lane, on every PR.
// ===========================================================================

/// This harness must not be able to touch `fbiy-B0`/`fbiy-B1`'s cluster, or the
/// developer's Tilt environment.
///
/// Three separate facts, because any one of them alone is satisfiable by a bug:
/// the script ACCEPTS this file's triple (so the live half targets something
/// that can exist); the script's own DEFAULT cluster is a different one (so the
/// two harnesses are genuinely disjoint rather than accidentally equal today);
/// and a foreign context against this cluster name is refused outright.
#[test]
fn guard_this_harness_is_disjoint_from_the_b0_defaults_and_the_tilt_cluster() {
    let accepted = run_setup_script(&[
        "check",
        "--cluster-name",
        HARNESS_CLUSTER,
        "--registry-name",
        HARNESS_REGISTRY,
        "--registry-port",
        HARNESS_REGISTRY_PORT,
        "--context",
        HARNESS_CONTEXT,
    ]);
    assert_eq!(
        exit_code(&accepted),
        0,
        "the script must accept this file's cluster/registry/port/context; stderr: {}",
        stderr(&accepted),
    );
    assert!(
        stdout(&accepted).contains(&format!("cluster={HARNESS_CLUSTER} "))
            && stdout(&accepted).contains(&format!("context={HARNESS_CONTEXT} ")),
        "the script must derive this file's context from its cluster name, got: {}",
        stdout(&accepted),
    );

    // Disjointness from B0/B1, which run the script with its defaults.
    let defaults = run_setup_script(&["check"]);
    assert_eq!(exit_code(&defaults), 0, "stderr: {}", stderr(&defaults));
    assert!(
        !stdout(&defaults).contains(&format!("cluster={HARNESS_CLUSTER} ")),
        "this harness must NOT share the script's default cluster: `down` deletes what it is \
         given, so a shared name lets two concurrent harnesses destroy each other. Script \
         defaults: {}",
        stdout(&defaults),
    );
    assert!(
        !stdout(&defaults).contains(&format!(
            "registry={HARNESS_REGISTRY}:{HARNESS_REGISTRY_PORT}"
        )),
        "this harness must not share the script's default registry name or published port; \
         script defaults: {}",
        stdout(&defaults),
    );

    // A context this cluster name does not derive is refused, not "used anyway".
    for foreign in ["kind-djinn", "kind-djinn-kueue-harness", "staging", "prod"] {
        let refused = run_setup_script(&[
            "check",
            "--cluster-name",
            HARNESS_CLUSTER,
            "--context",
            foreign,
        ]);
        assert_eq!(
            exit_code(&refused),
            EXIT_REFUSED_TARGET,
            "context {foreign} must be refused with exit {EXIT_REFUSED_TARGET}; stderr: {}",
            stderr(&refused),
        );
    }
}

/// The renderer requests `cpu` and `memory` on every task-run Job.
///
/// This is the hermetic half of live finding 1: the chart's ClusterQueue covers
/// `["pods"]` alone, and Kueue refuses to assign flavors to a pod set that
/// requests a resource the queue does not cover. The live repair
/// ([`repair_cluster_queue_for_admission`]) exists ONLY because this is true, so
/// if the renderer ever stopped requesting them the repair — and the reasoning
/// behind it — would be stale, and this is where that shows up.
///
/// Deliberately NOT phrased as "the chart does not cover them": that assertion
/// would start failing the moment somebody fixes the chart, which is the
/// opposite of what a regression guard should do.
#[test]
fn guard_the_task_run_renderer_requests_cpu_and_memory() {
    let (job, _) = rendered_task_run_job(&armed_harness_config(WORKLOAD_IMAGE));
    let requests = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .map(|pod| &pod.containers)
        .and_then(|containers| containers.first())
        .and_then(|container| container.resources.as_ref())
        .and_then(|resources| resources.requests.as_ref())
        .expect("the task-run renderer sets container resource requests");
    let named: BTreeSet<&str> = requests.keys().map(String::as_str).collect();
    assert!(
        named.contains("cpu") && named.contains("memory"),
        "every rendered task-run Job requests cpu and memory, which is why a ClusterQueue \
         covering only `pods` can never admit one; requested: {named:?}",
    );
}

/// The invocation-lease governor's arming decision fails closed on every input
/// that is not an explicitly enforcing authority row.
///
/// AC3's bound is "the count holding a lifted `cpu.max` never exceeds K". That
/// count is the number of leases the FIFO granted AND whose authority says
/// `Lift`. If the authority projection could answer `Lift` for a missing or
/// unreadable row, the cap would be bounding a population that no longer needs
/// permission to lift, and the live assertion would be measuring nothing.
#[test]
fn guard_evaluate_invocation_lift_fails_closed_unless_the_authority_enforces() {
    assert_eq!(
        evaluate_invocation_lift(Err(())),
        InvocationLiftDecision::Unleased
    );
    assert_eq!(
        evaluate_invocation_lift(Ok(None)),
        InvocationLiftDecision::Unleased
    );
    for (mode, expected) in [
        (InvocationLeaseMode::Off, InvocationLiftDecision::Unleased),
        (InvocationLeaseMode::Shadow, InvocationLiftDecision::Shadow),
        (InvocationLeaseMode::Enforce, InvocationLiftDecision::Lift),
    ] {
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(InvocationLeaseAuthorityRow {
                epoch: 1,
                mode,
                cap: Some(1),
                updated_at: "2026-07-30T00:00:00Z".into(),
            }))),
            expected,
            "authority mode {mode:?} must project to {expected:?}",
        );
    }
}

/// The values fixture must leave room for a cap that BINDS.
///
/// AC3 requires `K < M`, where `M` is the number of admitted Workloads the
/// cluster can hold — bounded by the fixture's `kueue.buildPods` — and `K` is
/// the live invocation-lease cap. The live test derives `K = M - 1`. If the
/// fixture ever dropped `buildPods` to 1 then `K` would be 0, and a cap of 0
/// denies EVERY lease before `grant_next` even reads the queue: the live
/// assertion would go green while proving nothing about a cap at all.
#[test]
fn guard_the_fixture_pods_quota_leaves_room_for_a_binding_cap() {
    let build_pods = yaml_at(VALUES_FIXTURE)["kueue"]["buildPods"]
        .as_u64()
        .expect("kueue.buildPods is a number");
    assert!(
        build_pods >= 2,
        "the harness fixture must admit at least 2 Workloads so the live invocation-lease cap \
         K = M - 1 is at least 1; a K of 0 denies every lease before the queue is read and would \
         make AC3 vacuous. kueue.buildPods = {build_pods}",
    );
}

// ===========================================================================
// AC1 — force-delete the Pod, RETAIN the Job, record the disjunction
// ===========================================================================

/// The permitted disjunction, asserted as a disjunction, plus the maximum
/// transient live-Pod count as a REPORTED NUMBER.
///
/// The proposal hedges deliberately: the Workload MAY become `Finished`, usage
/// MAY fall, the old Job MAY create a replacement Pod, a newly admitted Job MAY
/// overlap. Asserting any single branch produces a flaky test that somebody
/// later weakens, so this asserts membership in the permitted set and RECORDS
/// which branch the cluster actually took.
///
/// Measured 2026-07-30 (Kubernetes 1.31, Kueue 0.19.0): Workload `Finished`,
/// `pods` usage falls to 0, NO replacement Pod, a new Job admits into the freed
/// quota — and the maximum transient live-Pod count is **1**. See finding 2 in
/// the module docs; it is why `backoffLimit: 0` is asserted at launch.
///
/// The load-bearing assertion is the one nothing about the disjunction can
/// satisfy by accident: `pods` usage never exceeds the nominal quota at any
/// sample, across the whole disruption.
#[test]
#[ignore]
fn live_force_deleting_an_admitted_pod_records_the_permitted_disjunction() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    let repair = repair_cluster_queue_for_admission(&context);
    eprintln!("repair needed: {} ({repair:?})", repair.needed());
    ensure_workload_image_on_the_node();

    let config = config_for(&context, WORKLOAD_IMAGE);
    let quota = pods_nominal_quota(&context);

    let (task_run_id, job_name) = launch_task_run(&context, &config);
    let (pod_name, bound_uid) = await_running_pod(&context, &task_run_id);
    assert!(
        workload_condition(&context, &job_name, "Admitted"),
        "the Job must be ADMITTED before it can be disrupted; workloads: {}",
        workload_summary(&context),
    );

    // Force-delete the worker Pod while RETAINING the Job.
    let deleted = Command::new("kubectl")
        .args([
            "--context",
            &context,
            "-n",
            NAMESPACE,
            "delete",
            "pod",
            &pod_name,
            "--grace-period=0",
            "--force",
        ])
        .output()
        .expect("kubectl is on PATH");
    assert!(
        deleted.status.success(),
        "force-deleting {pod_name} failed: {}",
        stderr(&deleted),
    );

    // A second run, launched into whatever capacity the disruption released.
    // This is the "a newly admitted Job MAY overlap" branch, and it is also what
    // makes the maximum-live-Pod census a measurement of something.
    let (overlap_task_run_id, overlap_job_name) = launch_task_run(&context, &config);

    let mut max_live_pods = 0usize;
    let mut max_pods_usage = 0u64;
    let mut replacement_uids: BTreeSet<String> = BTreeSet::new();
    let mut overlap_admitted = false;
    for _ in 0..120 {
        max_live_pods = max_live_pods.max(live_task_run_pods(&context).len());
        max_pods_usage = max_pods_usage.max(pods_usage(&context));
        for (_, uid, _) in pods_of(&context, &task_run_id) {
            if !uid.is_empty() && uid != bound_uid {
                replacement_uids.insert(uid);
            }
        }
        overlap_admitted |= workload_condition(&context, &overlap_job_name, "Admitted");
        std::thread::sleep(TICK);
    }

    let workload_finished = workload_condition(&context, &job_name, "Finished");
    let usage_fell = pods_usage(&context) < quota;
    let job_retained = job_exists(&context, &job_name);

    eprintln!(
        "AC1 RECORDED (never asserted equal to kueue.buildPods={quota}): maximum transient \
         live-Pod count = {max_live_pods}; maximum ClusterQueue pods usage = {max_pods_usage}"
    );
    eprintln!(
        "AC1 disjunction observed: workload_finished={workload_finished} usage_fell={usage_fell} \
         replacement_pod_uids={replacement_uids:?} overlapping_job_admitted={overlap_admitted} \
         old_job_object_retained={job_retained}"
    );

    // The disjunction: at least one of the permitted consequences of losing the
    // Pod must have happened. A cluster where the Workload stayed admitted, the
    // quota stayed spent, no replacement appeared AND nothing else could be
    // admitted has LEAKED the slot, which is the failure mode `fbiy` exists to
    // rule out.
    assert!(
        workload_finished || usage_fell || !replacement_uids.is_empty() || overlap_admitted,
        "losing an admitted task-run's Pod must have at least one permitted consequence — \
         otherwise the quota is leaked. workloads: {}",
        workload_summary(&context),
    );

    // The absolute invariant, sampled throughout rather than once at the end:
    // Kueue never reserves more `pods` than it nominally has.
    assert!(
        max_pods_usage <= quota,
        "ClusterQueue pods usage reached {max_pods_usage}, above its own nominalQuota {quota}",
    );
    assert!(
        max_live_pods >= 1,
        "the census never saw a live task-run Pod, so it measured nothing",
    );

    delete_job(&context, &job_name);
    delete_job(&context, &overlap_job_name);
    let _ = overlap_task_run_id;
}

// ===========================================================================
// AC2 — the replacement UID, and the REAL watch that refuses it
// ===========================================================================

/// The live subject AC2 needs, produced by the only thing that produces it.
///
/// A force-deleted Pod mints no replacement (finding 2). A Kueue EVICTION does:
/// it re-suspends the Job, deletes the Pod, and — when capacity returns —
/// re-admits the same Workload and the Job controller creates a NEW Pod with a
/// DIFFERENT `metadata.uid` under the very same labels. That is the object
/// `fenced_worker_pod` must refuse to adopt, and this drives the REAL
/// `SessionRuntime::watch_infra_death` at it rather than a copy of its logic.
///
/// Three assertions, each of which fails on a different removal:
///
/// 1. the watch RESOLVES — reverting A1's Pod-absent-plus-Job-nonterminal arm
///    makes it hang until the timeout, because the evicted Job is neither
///    `Failed` nor `Complete` and the pre-existing arms have nothing to say;
/// 2. the reason names the ORIGINAL Pod UID — removing the UID comparison from
///    `fenced_worker_pod` makes the watch adopt the re-admitted Pod, see it
///    healthy, and never resolve at all;
/// 3. the Job is GONE from the live API server — a reap that builds
///    `DeleteParams` and never sends them leaves it there.
#[test]
#[ignore]
fn live_a_kueue_eviction_produces_the_replacement_uid_the_watch_refuses() {
    if !live_tests_enabled() {
        return;
    }
    install_crypto_provider();
    let context = harness_context();
    clear_task_run_jobs(&context);
    repair_cluster_queue_for_admission(&context);
    ensure_workload_image_on_the_node();
    let config = config_for(&context, WORKLOAD_IMAGE);

    let (task_run_id, job_name) = launch_task_run(&context, &config);
    let (_, bound_uid) = await_running_pod(&context, &task_run_id);
    assert!(
        workload_condition(&context, &job_name, "Admitted"),
        "the run must be admitted before eviction can mean anything; workloads: {}",
        workload_summary(&context),
    );

    let handle = RunHandle {
        task_run_id: task_run_id.clone(),
        container_id: None,
        // The K8s runtime carries the JOB name here; see
        // `KubernetesRuntime::prepare`.
        pod_ref: Some(job_name.clone()),
        // `SystemTime::now` is workspace-disallowed (`clippy.toml`); this is the
        // same clock `KubernetesRuntime::prepare` stamps a real handle with.
        started_at: SystemClock::new().now(),
    };

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build a tokio runtime for the live watch");
    let reason = tokio_runtime.block_on({
        let context = context.clone();
        let task_run_id = task_run_id.clone();
        let bound_uid = bound_uid.clone();
        let config = config.clone();
        async move {
            let runtime = KubernetesRuntime::from_client(
                kube_client(&context).await,
                config,
                std::sync::Arc::new(ConnectionRegistry::new()),
            );
            let evicting = tokio::task::spawn_blocking(move || {
                // Let the watch bind its immutable fence to the Pod that exists
                // NOW. A watch disrupted before it has ever observed a Pod has
                // nothing to be fenced to and would pass trivially.
                std::thread::sleep(Duration::from_secs(20));
                evict_and_release(&context, &task_run_id, &bound_uid);
            });
            let reason =
                tokio::time::timeout(Duration::from_secs(300), runtime.watch_infra_death(&handle))
                    .await;
            let _ = evicting.await;
            reason
        }
    });
    // Whatever happened, do not leave the queue stopped for the next test.
    set_stop_policy(&context, "None");

    let reason = reason.unwrap_or_else(|_| {
        panic!(
            "the infra-death watch never resolved across a Kueue eviction. The evicted Job is \
             suspended, NOT Failed and NOT Complete, so only fbiy-A1's \
             Pod-absent-plus-Job-nonterminal arm can resolve it. Job still present: {}; \
             workloads: {}",
            job_exists(&context, &job_name),
            workload_summary(&context),
        )
    });
    eprintln!("AC2 live death reason: {reason}");

    assert!(
        reason.contains(&bound_uid),
        "the run stays bound to the Pod UID it launched — the watch must name it rather than \
         adopt whatever Pod the re-admission created. bound_uid={bound_uid}, reason: {reason}",
    );

    // The replacement, when the re-admission got one in front of a poll. This is
    // RECORDED rather than required, because the watch may legitimately resolve
    // inside the eviction window before Kueue re-admits — both orderings are
    // correct behaviour and asserting one produces a flaky test.
    let observed_uids: BTreeSet<String> = pods_of(&context, &task_run_id)
        .into_iter()
        .map(|(_, uid, _)| uid)
        .filter(|uid| !uid.is_empty() && *uid != bound_uid)
        .collect();
    let refused_by_name = reason.contains("refused to adopt replacement Pod UID(s)");
    eprintln!(
        "AC2 RECORDED: replacement Pod UIDs seen after re-admission = {observed_uids:?}; \
         the watch reported them refused by name = {refused_by_name}"
    );
    for uid in &observed_uids {
        assert_ne!(
            uid, &bound_uid,
            "a replacement Pod must carry a different immutable UID",
        );
    }

    // The reap, observed on the API server rather than in a log line.
    let mut reaped = false;
    for _ in 0..AWAIT_TICKS {
        if !job_exists(&context, &job_name) {
            reaped = true;
            break;
        }
        std::thread::sleep(TICK);
    }
    assert!(
        reaped,
        "reconciliation must foreground-delete the old Job so it stops holding quota before the \
         task-run is retried; {job_name} is still on the API server",
    );

    delete_job(&context, &job_name);
}
