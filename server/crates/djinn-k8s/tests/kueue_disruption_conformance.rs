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
//!    RECOVERABLE — and the containment deleted the Job anyway. That was filed
//!    as `03z3`, a P0 blocker on arming Kueue anywhere, and fixed by narrowing
//!    the arm to an absence nothing in observable cluster state explains
//!    (`crate::runtime_eviction`). The test that used to assert the reap here is
//!    replaced by [`live_a_kueue_eviction_and_re_admission_leaves_the_task_run_alive`],
//!    which asserts the opposite, plus the two fields it turns on. Measured
//!    2026-07-31: the Job reads `suspend: true` from t+0s of the eviction while
//!    the Pod took 34s to go, and the re-admitted Workload STILL carries its
//!    `Evicted` condition, flipped to `False` with message
//!    `Previously: The ClusterQueue is stopped`.
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
// `03z3` — an eviction is RECOVERABLE, and the watch must let it recover
// ===========================================================================

/// **This test replaces the one that codified the defect.**
///
/// `fbiy-B2` measured that A1's containment arm is reached on a live cluster
/// almost exclusively BY EVICTION (finding 3 above: a force-delete goes
/// `Failed` in ~5s under `backoffLimit: 0` and resolves through the pre-existing
/// arm, so it never reaches A1's at all). The test that used to live here
/// asserted the consequence — the watch resolves, the Job is reaped — and that
/// consequence is wrong: releasing the queue re-admits the same Workload and the
/// run can still finish, so reaping there converts a recoverable task-run into a
/// destroyed one. `03z3` is that defect; this is its live proof.
///
/// Four assertions, each failing on a different regression:
///
/// 1. the watch does NOT resolve — restore the unconditional foreground delete
///    and it resolves inside the eviction window, which is how the dispatch
///    runner terminalises a run;
/// 2. the Job is STILL on the API server — a reaped Job can never be re-admitted
///    and no retry ever adopts it (a retry mints a fresh task-run id);
/// 3. Kueue re-admitted it and a NEW Pod is Running — so this measured a
///    recovery rather than a cluster that quietly stayed broken;
/// 4. AC3's distinguisher is asserted as a FIELD READ, in both phases: the Job
///    reads `suspend: true` during the eviction, and the Workload still carries
///    its `Evicted` condition after re-admission — which is the one the watch
///    can still see at the sample a 15s poll actually takes.
#[test]
#[ignore]
fn live_a_kueue_eviction_and_re_admission_leaves_the_task_run_alive() {
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
    assert_eq!(
        workload_condition_entry(&context, &job_name, "Evicted"),
        None,
        "an admitted Workload that was never evicted carries no Evicted condition at all — that \
         is what makes its later presence evidence rather than decoration",
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
        job_uid: None,
        launcher_authority_protocol: None,
    };

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build a tokio runtime for the live watch");
    let (resolved, observed) = tokio_runtime.block_on({
        let context = context.clone();
        let task_run_id = task_run_id.clone();
        let job_name = job_name.clone();
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
                evict_capturing_the_distinguisher(&context, &task_run_id, &job_name, &bound_uid)
            });
            // The watch must still be RUNNING when this elapses. The budget is
            // deliberately far past the whole eviction: measured 2026-07-31, the
            // drain took ~34s and the re-admission ~1s after release, against a
            // 15s poll — so this leaves the watch several polls on each side,
            // including the post-re-admission samples that are the destructive
            // ones.
            let resolved =
                tokio::time::timeout(Duration::from_secs(150), runtime.watch_infra_death(&handle))
                    .await;
            let observed = evicting.await.expect("the eviction driver panicked");
            (resolved, observed)
        }
    });
    // Whatever happened, do not leave the queue stopped for the next test.
    set_stop_policy(&context, "None");

    eprintln!("03z3 live eviction observation: {observed:?}");

    // 1. The watch never resolved. `Elapsed` is the pass.
    if let Ok(reason) = resolved {
        panic!(
            "a routine Kueue eviction terminalised a RECOVERABLE task-run: the infra-death watch \
             resolved with {reason:?}. Job still present: {}; workloads: {}",
            job_exists(&context, &job_name),
            workload_summary(&context),
        );
    }

    // 2. The Job survived, so the run is still the one Kueue re-admitted.
    assert!(
        job_exists(&context, &job_name),
        "the evicted Job must survive: deleting it is precisely what makes a recoverable \
         eviction unrecoverable. Workloads: {}",
        workload_summary(&context),
    );

    // 3. The recovery is real, not a cluster that stayed broken quietly.
    let recovered: BTreeSet<String> = pods_of(&context, &task_run_id)
        .into_iter()
        .filter(|(_, uid, phase)| !uid.is_empty() && *uid != bound_uid && phase == "Running")
        .map(|(_, uid, _)| uid)
        .collect();
    assert!(
        !recovered.is_empty(),
        "Kueue must have re-admitted the Workload and the Job controller must have created a NEW \
         Pod; pods now: {:?}, workloads: {}",
        pods_of(&context, &task_run_id),
        workload_summary(&context),
    );
    assert!(
        workload_condition(&context, &job_name, "Admitted"),
        "the re-admitted Workload must be Admitted again; workloads: {}",
        workload_summary(&context),
    );

    // 4. AC3: the distinguisher, asserted as a field read in both phases.
    assert!(
        observed.suspended_during,
        "Kueue re-suspends an evicted Job — `spec.suspend` is the Job-level half of the \
         distinguisher and it was never observed true: {observed:?}",
    );
    let (during_status, during_reason) = observed
        .evicted_during
        .clone()
        .expect("an evicted Workload carries an Evicted condition");
    assert_eq!(
        during_status, "True",
        "during the eviction the Workload's Evicted condition is True (reason {during_reason})",
    );
    let (after_status, after_reason) = observed.evicted_after_readmission.clone().expect(
        "THE FIELD THIS FIX TURNS ON: Kueue leaves the Evicted condition behind after \
             re-admission. Without it, a poll landing after the release sees an unsuspended Job \
             with its fenced Pod gone — bit for bit the state fbiy-A1 reaps on — and nothing to \
             tell it apart from abandonment",
    );
    assert!(
        !observed.suspended_after_readmission,
        "the re-admitted Job is unsuspended ({observed:?}), which is exactly why `spec.suspend` \
         alone cannot carry this and the Workload's record has to",
    );
    eprintln!(
        "03z3 AC3 distinguisher: Evicted during = ({during_status}, {during_reason}); after \
         re-admission = ({after_status}, {after_reason}); job suspend during = {}, after = {}",
        observed.suspended_during, observed.suspended_after_readmission,
    );
    assert_eq!(
        observed.pods_usage_during, 0,
        "RECORDED and asserted: an evicted Workload holds NO ClusterQueue quota, which is why \
         holding the reap off does not strand the capacity the reap exists to release",
    );

    delete_job(&context, &job_name);
}

/// AC2, live: the narrowing must not have disarmed a genuine Pod loss.
///
/// On this cluster a force-delete resolves through the PRE-EXISTING
/// `job_failed_reason` arm rather than A1's (finding 2), and that is the point:
/// whichever arm answers, the run must still terminalise rather than hang. A fix
/// that held on every absence — the obvious way to make the eviction test pass —
/// fails here, because nothing else would ever resolve this watch.
#[test]
#[ignore]
fn live_a_force_deleted_pod_still_terminalises_the_run() {
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
    let (pod_name, bound_uid) = await_running_pod(&context, &task_run_id);
    assert_eq!(
        workload_condition_entry(&context, &job_name, "Evicted"),
        None,
        "this run is being force-deleted, not evicted: its Workload must carry no Evicted record",
    );

    let handle = RunHandle {
        task_run_id: task_run_id.clone(),
        container_id: None,
        pod_ref: Some(job_name.clone()),
        started_at: SystemClock::new().now(),
        job_uid: None,
        launcher_authority_protocol: None,
    };
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build a tokio runtime for the live watch");
    let resolved = tokio_runtime.block_on({
        let context = context.clone();
        let config = config.clone();
        async move {
            let runtime = KubernetesRuntime::from_client(
                kube_client(&context).await,
                config,
                std::sync::Arc::new(ConnectionRegistry::new()),
            );
            let destroying = tokio::task::spawn_blocking(move || {
                std::thread::sleep(Duration::from_secs(20));
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
            });
            let resolved =
                tokio::time::timeout(Duration::from_secs(180), runtime.watch_infra_death(&handle))
                    .await;
            let _ = destroying.await;
            resolved
        }
    });

    let reason = resolved.unwrap_or_else(|_| {
        panic!(
            "a force-deleted worker Pod must still terminalise its run. Job present: {}; \
             workloads: {}",
            job_exists(&context, &job_name),
            workload_summary(&context),
        )
    });
    eprintln!("03z3 AC2 live death reason (bound uid {bound_uid}): {reason}");

    delete_job(&context, &job_name);
}
