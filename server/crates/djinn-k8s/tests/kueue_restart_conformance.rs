// Test: eprintln is how the RECORDED (never asserted) live measurements of this
// file reach the operator reading the run, plus the skip-reason channel for the
// gated half. Mirrors tests/kueue_disruption_conformance.rs.
#![allow(clippy::print_stderr)]
//! Coordinator-restart interleavings on the LIVE armed-Kueue cluster
//! (`fbiy-B2b` / `c6ej` AC4 + AC5).
//!
//! `fbiy-B2a` (#2842) proved disruption CONTAINMENT against a live cluster and
//! deliberately deferred the restart half, because killing a coordinator
//! between Job creation and Workload admission requires a coordinator and that
//! harness has none (Postgres disabled, `djinn-server` in `ImagePullBackOff`).
//! This file brings one: a real [`KubernetesRuntime`] bound to a real Postgres,
//! dispatching through the real [`SessionRuntime::prepare`], destroyed, and
//! rebuilt from scratch — which is what a coordinator restart *is*.
//!
//! WHAT A "RESTART" IS HERE, AND WHY IT IS NOT A MOCK
//! -------------------------------------------------
//! A restarted coordinator loses every in-process fact: its `kube::Client`, its
//! `ConnectionRegistry`, its pending-connection map, and the [`RunHandle`]
//! `prepare` returned. It keeps exactly one thing — the durable `task_run_id`
//! it minted BEFORE dispatch and persisted on the task-run row
//! (`TaskRunSpec::task_run_id`: "minted once by the host coordinator before
//! `prepare`"). So the restart is modelled by dropping the whole runtime and
//! its handle on the floor and building a new one from a new client, then
//! re-dispatching the SAME spec. Nothing is faked, nothing is re-implemented:
//! the convergence under test is `create_or_adopt_task_run_job`'s
//! `AlreadyExists` → GET-and-adopt arm, reached through `prepare` itself.
//!
//! WHAT THE B2a FINDINGS COST THIS FILE
//! ------------------------------------
//! B2a measured two live facts that overturn the proposal's premises, and both
//! are load-bearing here rather than re-derived:
//!
//! 1. **A force-deleted Pod mints no replacement.** `backoffLimit: 0` turns the
//!    first Pod loss into an immediate Job failure. So the disruption repeated
//!    across the restart below is NOT a Pod force-delete — that would leave a
//!    `Failed` Job and nothing to converge to.
//! 2. **A Kueue eviction is the disruption that leaves a recoverable run.** It
//!    re-suspends the Job and deletes its Pod without marking it `Failed`;
//!    releasing the queue re-admits the same Workload with a NEW Pod UID. That
//!    is B2a's disruption scenario, and it is the one interleaving (b) repeats
//!    across the restart.
//!
//! Blocker `03z3` is fixing `watch_infra_death` so that arm stops destroying a
//! recoverable eviction. This file's assertions describe the POST-FIX
//! behaviour: after an eviction and release, a restarted coordinator's run
//! converges to one Running Pod rather than to a reaped Job. It does not itself
//! run `watch_infra_death` — the watch is A1/`03z3`'s subject, and running it
//! here would make AC4's convergence assertions a measurement of that fix
//! instead of of the restart.
//!
//! GATING AND ISOLATION
//! --------------------
//! Same split every `fbiy-B*` harness uses: `live_*` tests are `#[ignore]` +
//! `DJINN_TEST_KUEUE_CLUSTER=1` and NO CI lane runs them (`fbiy-B0`'s lane is
//! `workflow_dispatch`-only), so everything that must not regress silently is a
//! `guard_*` test with no `#[ignore]`, running in the ordinary
//! `cargo test -p djinn-k8s` lane.
//!
//! This file owns its OWN cluster, registry and port — distinct from the
//! script's defaults AND from B2a's (`djinn-kueue-b2`, port 5053) — because
//! `down` DELETES what it is given and several harnesses run concurrently.
//! Kept that way by [`guard_this_harness_is_disjoint_from_every_other_kueue_harness`].
//! Only the context-free helpers of `tests/kueue_disruption/mod.rs` are reused
//! (every one of them takes the context as a parameter); the cluster-bound ones
//! are re-declared here against this file's own triple.
//!
//! ```bash
//! scripts/kind/setup-kueue-cluster.sh up --cluster-name djinn-kueue-b2b \
//!     --registry-name djinn-kueue-b2b-registry --registry-port 5055 \
//!     --context kind-djinn-kueue-b2b
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s \
//!     --test kueue_restart_conformance -- --ignored --test-threads=1
//! scripts/kind/setup-kueue-cluster.sh down --cluster-name djinn-kueue-b2b \
//!     --registry-name djinn-kueue-b2b-registry
//! ```

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::process::Command;
use std::sync::Arc;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
    GrantNextBuildLeaseResult, ImageRepository, InvocationLeaseAuthorityRepository,
    InvocationLeaseMode, ProjectRepository, QueueBuildLeaseInput,
};
use djinn_k8s::KubernetesConfig;
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_runtime::{
    ResolvedCredentials, RunHandle, SessionRuntime, SupervisorFlow, TaskRunSpec,
};
use djinn_supervisor::ConnectionRegistry;
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};
use serde_json::Value;

mod kueue_disruption;
use kueue_disruption::{
    AWAIT_TICKS, CLUSTER_QUEUE, NAMESPACE, TICK, VALUES_FIXTURE, admitted_workloads,
    clear_task_run_jobs, delete_job, evict_and_release, exit_code, install_crypto_provider,
    job_exists, kube_client, kubectl_json, kubectl_raw, live_tests_enabled, pods_of,
    rendered_task_run_job, repair_cluster_queue_for_admission, run_setup_script, set_stop_policy,
    stderr, stdout, which, workload_summary, yaml_at,
};

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch.
// ---------------------------------------------------------------------------

/// DELIBERATELY distinct from the script's default (`djinn-kueue-harness`) AND
/// from `fbiy-B2a`'s (`djinn-kueue-b2`). `down` deletes the cluster it is given,
/// so a shared name is a shared deletion between two concurrent agents.
const RESTART_CLUSTER: &str = "djinn-kueue-b2b";
/// kind names its context `kind-<cluster>`. Derived rather than read from the
/// kubeconfig's CURRENT context, because every context in a Djinn developer's
/// kubeconfig is a live EKS cluster and these tests DELETE objects.
const RESTART_CONTEXT: &str = "kind-djinn-kueue-b2b";
const RESTART_REGISTRY: &str = "djinn-kueue-b2b-registry";
const RESTART_REGISTRY_PORT: &str = "5055";

/// The repository the stub worker image is pushed to, inside this harness's own
/// registry. `setup-kueue-cluster.sh` wires the node's containerd to resolve
/// `localhost:<port>/...` through `http://<registry>:5000`, so a digest-pinned
/// ref of this repository is pullable from inside the cluster.
const STUB_IMAGE_REPO: &str = "djinn-b2b-worker-stub";

/// The exit code `scripts/kind/setup-kueue-cluster.sh` reserves for a refused
/// context or a reserved name.
const EXIT_REFUSED_TARGET: i32 = 3;

const PROJECT_ID: &str = "fbiy-b2b-project";
const IMAGE_ID: &str = "fbiy-b2b-image";

// ===========================================================================
// Hermetic guards — these run in the ordinary test lane, on every PR.
// ===========================================================================

/// This harness must not be able to touch `fbiy-B0`/`B1`'s cluster, `B2a`'s
/// cluster, or the developer's Tilt environment.
///
/// Four separate facts, because any one of them alone is satisfiable by a bug:
/// the script ACCEPTS this file's triple (so the live half targets something
/// that can exist); the script's DEFAULT triple is a different one; B2a's
/// triple is a different one; and a foreign context against this cluster name
/// is refused outright rather than "used anyway".
#[test]
fn guard_this_harness_is_disjoint_from_every_other_kueue_harness() {
    let accepted = run_setup_script(&[
        "check",
        "--cluster-name",
        RESTART_CLUSTER,
        "--registry-name",
        RESTART_REGISTRY,
        "--registry-port",
        RESTART_REGISTRY_PORT,
        "--context",
        RESTART_CONTEXT,
    ]);
    assert_eq!(
        exit_code(&accepted),
        0,
        "the script must accept this file's cluster/registry/port/context; stderr: {}",
        stderr(&accepted),
    );
    assert!(
        stdout(&accepted).contains(&format!("cluster={RESTART_CLUSTER} "))
            && stdout(&accepted).contains(&format!("context={RESTART_CONTEXT} ")),
        "the script must derive this file's context from its cluster name, got: {}",
        stdout(&accepted),
    );

    let defaults = run_setup_script(&["check"]);
    assert_eq!(exit_code(&defaults), 0, "stderr: {}", stderr(&defaults));
    assert!(
        !stdout(&defaults).contains(&format!("cluster={RESTART_CLUSTER} ")),
        "this harness must NOT share the script's default cluster; script defaults: {}",
        stdout(&defaults),
    );
    assert!(
        !stdout(&defaults).contains(&format!(
            "registry={RESTART_REGISTRY}:{RESTART_REGISTRY_PORT}"
        )),
        "this harness must not share the script's default registry name or published port; \
         script defaults: {}",
        stdout(&defaults),
    );

    // B2a's triple, spelled out rather than imported: the point of this
    // assertion is that the two are different, and importing the constant
    // would make it pass by construction if B2a ever adopted this one.
    assert_ne!(RESTART_CLUSTER, "djinn-kueue-b2");
    assert_ne!(RESTART_REGISTRY, "djinn-kueue-b2-registry");
    assert_ne!(RESTART_REGISTRY_PORT, "5053");

    for foreign in [
        "kind-djinn",
        "kind-djinn-kueue-b2",
        "kind-djinn-kueue-harness",
        "staging",
        "prod",
    ] {
        let refused = run_setup_script(&[
            "check",
            "--cluster-name",
            RESTART_CLUSTER,
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

/// The task-run Job name is a pure function of the durable task-run id.
///
/// This is the hermetic half of the whole restart premise. A restarted
/// coordinator re-dispatches the SAME `task_run_id`, and the ONLY reason that
/// converges on one Job instead of two is that the renderer derives the object
/// name from that id — so the second POST collides and
/// `create_or_adopt_task_run_job` can adopt. If the name ever became
/// non-deterministic (a timestamp, a random suffix, a retry counter) every
/// restart would mint a second Job, a second Workload and a second Pod, and the
/// live assertions below would be the only thing standing in the way. This
/// fires first, in ordinary CI.
#[test]
fn guard_the_task_run_job_name_is_a_pure_function_of_the_task_run_id() {
    let config = restart_harness_config("registry.invalid/never-pulled:1");
    let (first, first_id) = rendered_task_run_job(&config);
    let (second, second_id) = rendered_task_run_job(&config);
    let name_of = |job: &k8s_openapi::api::batch::v1::Job| {
        job.metadata.name.clone().expect("the renderer names the Job")
    };
    assert_ne!(first_id, second_id, "each render mints a fresh task-run id");
    assert_ne!(
        name_of(&first),
        name_of(&second),
        "two DIFFERENT task-run ids must not collide on one Job name",
    );
    assert!(
        name_of(&first).contains(&first_id),
        "the Job name must carry its task-run id ({first_id}), or a restarted coordinator \
         re-dispatching the same id cannot collide with the object it already created; got {}",
        name_of(&first),
    );
    assert!(
        name_of(&first).len() <= 63,
        "the Job name must be a valid DNS-1123 label; got {}",
        name_of(&first),
    );
}

/// The stub worker image installs the binary the renderer actually invokes.
///
/// The live tests replace the worker's IMAGE (a sleep stub, so Pod identity is
/// measurable) but never its COMMAND — that stays the renderer's. If the
/// renderer ever moved the entrypoint, the stub would install a file nothing
/// executes, every Pod would `CreateContainerError`, and "no Running Pod
/// appeared" would look like an admission defect instead of a stale fixture.
/// The Dockerfile is therefore GENERATED from this path rather than carrying a
/// copy of it, and this guard is what makes that generation legible.
#[test]
fn guard_the_stub_image_installs_the_binary_the_renderer_invokes() {
    let path = rendered_worker_command_path(&restart_harness_config(
        "registry.invalid/never-pulled:1",
    ));
    assert!(
        path.starts_with('/'),
        "the rendered worker command must be an absolute path for the stub Dockerfile to \
         install it; got {path}",
    );
    let dockerfile = stub_dockerfile(&path);
    assert!(
        dockerfile.contains(&format!("> {path}")) && dockerfile.contains(&format!("chmod 0755 {path}")),
        "the generated Dockerfile must install AND make executable exactly the rendered \
         command path {path}; got:\n{dockerfile}",
    );
}

/// The invocation-lease governor's arming decision fails closed on every input
/// that is not an explicitly enforcing authority row.
///
/// `c6ej` AC5 requires this suite to fail if the cap is deleted. A cap only
/// bounds the population that is actually allowed to lift, so if the authority
/// projection could answer `Lift` for a missing or unreadable row the cap would
/// be bounding a set that no longer needs permission at all, and the live
/// assertion would be measuring nothing.
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
            evaluate_invocation_lift(Ok(Some(djinn_db::InvocationLeaseAuthorityRow {
                epoch: 1,
                mode,
                cap: Some(1),
                updated_at: "2026-07-31T00:00:00Z".into(),
            }))),
            expected,
            "authority mode {mode:?} must project to {expected:?}",
        );
    }
}

/// `c6ej` AC5, carried into the lane that actually runs: the durable invocation
/// governor bounds concurrent lifts at the cap AND fences each lease to the Pod
/// UID it was bound to.
///
/// The live half ([`live_restart_between_admission_and_the_task_state_write_converges`])
/// drives exactly this against two Pod UIDs Kubernetes minted. This one drives
/// it against a real Postgres in ordinary CI, so all four deletions AC5 names
/// break a lane that runs on every PR rather than one nobody triggers:
///
/// * delete the **invocation queue** (`grant_next`'s queue read) → nothing is
///   granted → `authorized == cap` fails at 0;
/// * delete the **weighted occupancy SUM** → every queued lease fits → both are
///   granted → `authorized <= cap` fails at 2;
/// * delete the **cap** comparison → same, `authorized <= cap` fails;
/// * delete **`bound_pod_uid` authorization** → rebinding the same lease to the
///   other UID stops being refused → the fence assertion fails.
///
/// Two leases against a cap of one, because a cap that is not strictly below
/// demand bounds nothing and every assertion under it would be vacuous.
#[tokio::test]
async fn guard_the_invocation_governor_bounds_the_cap_and_fences_the_pod_uid() {
    let db = Database::open_in_memory().expect("real Postgres test database");
    let outcome = drive_invocation_governor(
        &db,
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
    )
    .await;
    outcome.assert_bounded_and_fenced();
}

/// The values fixture must leave room for a `pods` quota that can hold the one
/// Workload these tests converge on.
///
/// Every live assertion below reads "used quota EQUALS this single Workload's
/// request". If the fixture's `kueue.buildPods` were 0 the Workload could never
/// be admitted at all and the convergence would be a statement about an empty
/// cluster; if it were somehow negative the comparison would be meaningless.
#[test]
fn guard_the_fixture_admits_at_least_one_build_pod() {
    let build_pods = yaml_at(VALUES_FIXTURE)["kueue"]["buildPods"]
        .as_u64()
        .expect("kueue.buildPods is a number");
    assert!(
        build_pods >= 1,
        "the harness fixture must admit at least one build Pod, or no restart interleaving can \
         converge on a Running Pod at all. kueue.buildPods = {build_pods}",
    );
}

// ===========================================================================
// AC4 (a) — killed between Job creation and Workload admission
// ===========================================================================

/// A coordinator killed after the Job POST and BEFORE Kueue admitted it
/// converges, after restart, on exactly one Pod, one Workload and one
/// Workload's worth of quota.
///
/// The kill point is made deterministic rather than raced: the ClusterQueue is
/// put in `stopPolicy: Hold` before the first dispatch, so Kueue captures the
/// Workload and provably does not admit it. The window is then asserted — no
/// `Admitted` condition, no Pod — because a "restart between creation and
/// admission" that actually happened after admission would be interleaving (b)
/// wearing a different name, and would pass for the wrong reason.
///
/// NON-VACUITY. The three assertions in [`assert_converged_to_exactly_one`] all
/// fire if the restarted coordinator creates a second Job instead of adopting
/// the first — verified by mutation, both from the test side
/// ([`live_a_second_dispatch_under_a_fresh_id_breaks_every_convergence_assertion`],
/// which runs in this same suite) and from the production side by replacing
/// `create_or_adopt_task_run_job`'s adopt arm with a second POST. A body that
/// did nothing at all would leave `prepare` returning the `AlreadyExists` error
/// and this test panics at the re-dispatch instead.
#[test]
#[ignore]
fn live_restart_between_job_creation_and_admission_converges() {
    let Some(mut world) = LiveWorld::open() else {
        return;
    };

    // --- Kill point (a): admission is held OPEN-LOOP until after the restart.
    set_stop_policy(&world.context, "Hold");

    let first = world.dispatch();
    eprintln!("AC4(a) first dispatch: job={} handle={:?}", world.job_name, first.pod_ref);

    // The window itself, asserted rather than assumed.
    let captured = world.await_workload_for_job();
    assert!(
        !workload_is_admitted(&world.context, &world.job_name),
        "the kill point must be BEFORE admission: Workload {captured} is already admitted while \
         the ClusterQueue is held. workloads: {}",
        workload_summary(&world.context),
    );
    assert!(
        pods_of(&world.context, &world.task_run_id).is_empty(),
        "a Workload that was never admitted must have no Pod; pods: {:?}",
        pods_of(&world.context, &world.task_run_id),
    );

    // --- The restart. Everything in-process is dropped, including the handle
    //     the coordinator never got to persist.
    world.restart(first);

    let second = world.dispatch();
    assert_eq!(
        second.pod_ref.as_deref(),
        Some(world.job_name.as_str()),
        "the restarted coordinator must return a handle for the SAME Job it adopted",
    );

    // --- Release admission and let the cluster converge.
    set_stop_policy(&world.context, "None");
    let (pod_name, pod_uid) = world.await_running_pod();
    eprintln!("AC4(a) converged on pod={pod_name} uid={pod_uid}");

    assert_converged_to_exactly_one(&world.context, &world.task_run_id, &world.job_name);
    world.cleanup();
}

// ===========================================================================
// AC4 (b) — killed between admission and the task-state write, then disrupted
// ===========================================================================

/// A coordinator killed after Kueue admitted the run and BEFORE it wrote any
/// durable task state converges, after restart AND after B2a's disruption,
/// on exactly one Pod, one Workload and one Workload's worth of quota — and the
/// task-state write it finally makes is fenced to the Pod that survived.
///
/// The disruption is a Kueue EVICTION, not a Pod force-delete: B2a measured
/// that `backoffLimit: 0` turns a force-deleted Pod into an immediate Job
/// failure with no replacement, so there would be nothing left to converge on.
/// An eviction re-suspends the Job, drops its Pod, and re-admits the same
/// Workload with a NEW Pod UID — a recoverable run, which is the state `03z3`
/// is fixing `watch_infra_death` to stop destroying. These assertions describe
/// that post-fix world: the run survives its own eviction.
///
/// The "task-state write" is not a fiction: it is the durable invocation lease
/// the coordinator binds to the live Pod UID. That makes the kill point real
/// (the row genuinely does not exist yet when the coordinator dies) and carries
/// `c6ej` AC5 into the live half — see
/// [`InvocationGovernorOutcome::assert_bounded_and_fenced`] for the four
/// deletions each assertion answers.
#[test]
#[ignore]
fn live_restart_between_admission_and_the_task_state_write_converges() {
    let Some(mut world) = LiveWorld::open() else {
        return;
    };

    let first = world.dispatch();
    let (_, admitted_uid) = world.await_running_pod();
    assert!(
        workload_is_admitted(&world.context, &world.job_name),
        "the kill point must be AFTER admission; workloads: {}",
        workload_summary(&world.context),
    );
    eprintln!("AC4(b) admitted before the kill: uid={admitted_uid}");

    // --- The restart, with NOTHING durable written about this dispatch.
    world.restart(first);
    let second = world.dispatch();
    assert_eq!(
        second.pod_ref.as_deref(),
        Some(world.job_name.as_str()),
        "the restarted coordinator must adopt the admitted Job rather than create another",
    );
    assert_converged_to_exactly_one(&world.context, &world.task_run_id, &world.job_name);

    // --- B2a's disruption scenario, repeated across the restart.
    evict_and_release(&world.context, &world.task_run_id, &admitted_uid);
    let (_, replacement_uid) = world.await_running_pod();
    assert_ne!(
        replacement_uid, admitted_uid,
        "a re-admitted Pod carries a different immutable UID, or the eviction never happened",
    );
    eprintln!("AC4(b) recovered from eviction: replacement uid={replacement_uid}");

    assert_converged_to_exactly_one(&world.context, &world.task_run_id, &world.job_name);

    // --- The task-state write the first coordinator never made, fenced to the
    //     Pod that actually survived. Two LIVE UIDs, a cap of one.
    let outcome = world
        .tokio
        .block_on(drive_invocation_governor(&world.db, &replacement_uid, &admitted_uid));
    outcome.assert_bounded_and_fenced();
    eprintln!(
        "AC4(b)/AC5 governor: queued={} authorized={} cap={}",
        outcome.queued, outcome.authorized, outcome.cap,
    );

    world.cleanup();
}

// ===========================================================================
// Non-vacuity, executed rather than described
// ===========================================================================

/// The mutation `c6ej` AC4 names — "let reconciliation create a second Job" —
/// run for real, with the three convergence assertions REQUIRED to fail.
///
/// A restarted coordinator that lost the durable task-run id would mint a new
/// one, and a fresh id renders a fresh Job name, so nothing collides and
/// nothing is adopted: two Jobs, two Workloads, two Pods, twice the quota. This
/// dispatches exactly that and asserts that
/// [`assert_converged_to_exactly_one`] PANICS — so the two interleavings above
/// cannot be passing because their assertions are unfalsifiable.
///
/// Deliberately phrased as "the assertion fires", not "two Pods appear": the
/// claim under test is about the assertion's sensitivity, and an assertion that
/// silently tolerated the second Job is exactly the failure this guards.
#[test]
#[ignore]
fn live_a_second_dispatch_under_a_fresh_id_breaks_every_convergence_assertion() {
    let Some(mut world) = LiveWorld::open() else {
        return;
    };

    let first = world.dispatch();
    let (_, original_uid) = world.await_running_pod();
    let original_job = world.job_name.clone();
    let original_task_run = world.task_run_id.clone();

    // The mutation: the "restarted" coordinator forgets the durable id.
    world.restart(first);
    world.remint_task_run_id();
    let _second = world.dispatch();
    let (_, second_uid) = world.await_running_pod();
    assert_ne!(original_uid, second_uid, "the mutation must produce a SECOND Pod");
    assert!(
        job_exists(&world.context, &original_job) && job_exists(&world.context, &world.job_name),
        "the mutation must leave BOTH Jobs on the API server",
    );

    let context = world.context.clone();
    let job_name = world.job_name.clone();
    for (label, task_run_id, job) in [
        ("the original dispatch", original_task_run, original_job.clone()),
        ("the second dispatch", world.task_run_id.clone(), job_name),
    ] {
        let context = context.clone();
        let fired = std::panic::catch_unwind(move || {
            assert_converged_to_exactly_one(&context, &task_run_id, &job);
        })
        .is_err();
        assert!(
            fired,
            "the convergence assertions must FIRE for {label} once a second Job exists; they did \
             not, so they are not measuring the thing AC4 asks for",
        );
    }

    delete_job(&world.context, &original_job);
    world.cleanup();
}

// ===========================================================================
// The assertions AC4 names
// ===========================================================================

/// Exactly one Running Pod, exactly one Workload referencing that Job, and a
/// ClusterQueue whose used quota equals that single Workload's request.
///
/// All three are read from the LIVE API server, and the Workload census is
/// namespace-wide on purpose: "exactly one Workload OWNED BY this Job" alone
/// would stay true while a second Job sat next to it holding a second slot,
/// which is precisely the leak a restart can cause. The quota comparison is the
/// one nothing can satisfy by accident — it is an equality against a number
/// derived from the Workload's own pod sets, not a constant.
fn assert_converged_to_exactly_one(context: &str, task_run_id: &str, job_name: &str) {
    let running: Vec<String> = pods_of(context, task_run_id)
        .into_iter()
        .filter(|(_, _, phase)| phase == "Running")
        .map(|(name, uid, _)| format!("{name}({uid})"))
        .collect();
    assert_eq!(
        running.len(),
        1,
        "exactly one Running Pod must carry task-run {task_run_id} after the restart; saw {running:?}",
    );

    let workloads = namespace_workloads(context);
    assert_eq!(
        workloads.len(),
        1,
        "exactly one Workload must exist after the restart; a second one is a leaked admission \
         slot. workloads: {}",
        workload_summary(context),
    );
    let owners: Vec<String> = workloads[0]["metadata"]["ownerReferences"]
        .as_array()
        .map(|owners| {
            owners
                .iter()
                .filter(|owner| owner["kind"] == "Job")
                .filter_map(|owner| owner["name"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        owners,
        vec![job_name.to_owned()],
        "the surviving Workload must reference the adopted Job {job_name}; it references {owners:?}",
    );

    let requested = workload_total_request(&workloads[0]);
    let used = cluster_queue_usage(context);
    assert_eq!(
        used, requested,
        "ClusterQueue used quota must equal the single surviving Workload's request. This is \
         where a duplicate admission shows up as a number rather than as an object. used={used:?} \
         requested={requested:?}",
    );
    eprintln!("CONVERGED: pods={running:?} workload_request={requested:?} queue_used={used:?}");
}

/// Whether the named Job's Workload carries `Admitted=True`.
fn workload_is_admitted(context: &str, job_name: &str) -> bool {
    namespace_workloads(context)
        .iter()
        .filter(|workload| workload_owner_is(workload, job_name))
        .any(|workload| {
            workload["status"]["conditions"]
                .as_array()
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"] == "Admitted" && condition["status"] == "True"
                    })
                })
        })
}

fn workload_owner_is(workload: &Value, job_name: &str) -> bool {
    workload["metadata"]["ownerReferences"]
        .as_array()
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner["kind"] == "Job" && owner["name"] == job_name)
        })
}

fn namespace_workloads(context: &str) -> Vec<Value> {
    kubectl_json(context, &["-n", NAMESPACE, "get", "workloads.kueue.x-k8s.io"])["items"]
        .as_array()
        .expect("a List has items")
        .clone()
}

/// The `pods` / `cpu` / `memory` a Workload asks the ClusterQueue for, summed
/// over its pod sets exactly the way Kueue does: per-Pod container requests
/// multiplied by the pod-set count.
///
/// Derived from the live object rather than from the renderer's constants, so
/// it stays an independent number to compare the queue's usage against.
fn workload_total_request(workload: &Value) -> BTreeMap<String, i64> {
    let mut total: BTreeMap<String, i64> = BTreeMap::new();
    for pod_set in workload["spec"]["podSets"]
        .as_array()
        .expect("a Workload declares pod sets")
    {
        let count = pod_set["count"].as_i64().unwrap_or(0);
        *total.entry("pods".into()).or_default() += count;
        for container in pod_set["template"]["spec"]["containers"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(requests) = container["resources"]["requests"].as_object() else {
                continue;
            };
            for (resource, quantity) in requests {
                let raw = quantity.as_str().unwrap_or("0");
                *total.entry(resource.clone()).or_default() +=
                    count * normalized_quantity(resource, raw);
            }
        }
    }
    total
}

/// The ClusterQueue's currently reserved quota, summed across flavors.
fn cluster_queue_usage(context: &str) -> BTreeMap<String, i64> {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    let mut total: BTreeMap<String, i64> = BTreeMap::new();
    for flavor in queue["status"]["flavorsUsage"]
        .as_array()
        .into_iter()
        .flatten()
    {
        for resource in flavor["resources"].as_array().into_iter().flatten() {
            let Some(name) = resource["name"].as_str() else {
                continue;
            };
            let raw = resource["total"].as_str().unwrap_or("0");
            *total.entry(name.to_owned()).or_default() += normalized_quantity(name, raw);
        }
    }
    // A resource the queue reports at zero and the Workload never requests must
    // not make the two maps unequal for a reason nobody cares about.
    total.retain(|_, value| *value != 0);
    total
}

/// `resource.Quantity` → a comparable integer, in the unit that resource is
/// naturally counted in (milli-CPU, bytes, whole pods).
///
/// Both sides of the equality go through this, because the API server
/// round-trips the SAME quantity in different spellings depending on where it
/// is written: a Workload keeps `100m`, while `flavorsUsage` may report `0.1`.
fn normalized_quantity(resource: &str, raw: &str) -> i64 {
    let raw = raw.trim();
    match resource {
        "cpu" => match raw.strip_suffix('m') {
            Some(milli) => milli.parse().unwrap_or_else(|_| panic!("cpu quantity {raw}")),
            None => {
                let cores: f64 = raw.parse().unwrap_or_else(|_| panic!("cpu quantity {raw}"));
                (cores * 1000.0).round() as i64
            }
        },
        "memory" | "ephemeral-storage" => {
            for (suffix, scale) in [
                ("Ki", 1024_i64),
                ("Mi", 1024 * 1024),
                ("Gi", 1024 * 1024 * 1024),
                ("Ti", 1024_i64.pow(4)),
                ("k", 1000),
                ("M", 1_000_000),
                ("G", 1_000_000_000),
            ] {
                if let Some(value) = raw.strip_suffix(suffix) {
                    return value
                        .parse::<i64>()
                        .unwrap_or_else(|_| panic!("memory quantity {raw}"))
                        * scale;
                }
            }
            raw.parse()
                .unwrap_or_else(|_| panic!("memory quantity {raw}"))
        }
        _ => raw
            .parse()
            .unwrap_or_else(|_| panic!("{resource} quantity {raw}")),
    }
}

// ===========================================================================
// The durable governor — c6ej AC5, shared by the hermetic and the live half
// ===========================================================================

fn invocation_key(pod_uid: &str) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: format!("invocation-{pod_uid}"),
    }
}

/// What one run of [`drive_invocation_governor`] measured.
struct InvocationGovernorOutcome {
    cap: i64,
    queued: usize,
    authorized: i64,
    fence_refused_the_other_uid: bool,
    binding_after_refusal: Option<String>,
    bound_uid: String,
}

impl InvocationGovernorOutcome {
    /// The four assertions `c6ej` AC5 names, each broken by a different
    /// deletion. See the doc on
    /// [`guard_the_invocation_governor_bounds_the_cap_and_fences_the_pod_uid`].
    fn assert_bounded_and_fenced(&self) {
        assert!(
            self.cap < i64::try_from(self.queued).expect("two leases fit an i64"),
            "the cap {} must be strictly below the {} queued leases, or it bounds nothing",
            self.cap,
            self.queued,
        );
        assert!(
            self.authorized <= self.cap,
            "{} invocations hold a lifted cpu.max, above the cap {}",
            self.authorized,
            self.cap,
        );
        assert_eq!(
            self.authorized, self.cap,
            "the cap must be REACHED as well as respected: {} of a possible {} were authorized, \
             which is what a deleted invocation queue looks like",
            self.authorized, self.cap,
        );
        assert!(
            self.fence_refused_the_other_uid,
            "a lift presenting a DIFFERENT Pod UID against a bound lease must be refused by the \
             bound_pod_uid fence",
        );
        assert_eq!(
            self.binding_after_refusal.as_deref(),
            Some(self.bound_uid.as_str()),
            "no rejected lift may have moved the durable Pod binding",
        );
    }
}

/// Queue two invocation leases against a cap of one, grant concurrently, bind
/// the winner to its Pod UID, then present the other UID against that same
/// lease.
///
/// Driven against a real PostgreSQL database rather than a mock repository
/// because the refusal is a locked read-modify-write plus a trigger-enforced
/// immutable column: a mock proves only that the Rust `if` was written.
async fn drive_invocation_governor(
    db: &Database,
    first_uid: &str,
    second_uid: &str,
) -> InvocationGovernorOutcome {
    let authority = InvocationLeaseAuthorityRepository::new(db.clone());
    let seeded = authority.seed_baseline().await.expect("seed the authority");
    authority
        .set_mode_and_cap(seeded.epoch, InvocationLeaseMode::Enforce, Some(1))
        .await
        .expect("arm the invocation-lease authority through the real operator API");

    // READ BACK. Everything below uses this value, never the one written.
    let live = authority
        .read()
        .await
        .expect("read the durable authority")
        .expect("the authority row exists once seeded");
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(live.clone()))),
        InvocationLiftDecision::Lift,
        "the live authority must actually authorize lifts, or the cap bounds a population that \
         never lifts and AC5 measures nothing",
    );
    let cap = live.cap.expect("an armed authority carries a reference cap");

    let repository = Arc::new(BuildLeaseRepository::new(db.clone()));
    let uids = [first_uid.to_owned(), second_uid.to_owned()];
    for uid in &uids {
        repository
            .queue(&QueueBuildLeaseInput {
                key: invocation_key(uid),
                immutable_identity: format!("pod:{uid}"),
                queue_deadline: None,
                launch_deadline: None,
                weight: 1,
            })
            .await
            .unwrap_or_else(|e| panic!("queue an invocation lease for {uid}: {e:?}"));
    }

    // Every grant the FIFO will make at the cap, taken concurrently so the
    // bound is a property of the locked transaction rather than of the order
    // this test happened to call in.
    let mut attempts = tokio::task::JoinSet::new();
    for _ in 0..uids.len() {
        let repository = repository.clone();
        attempts.spawn(async move {
            repository
                .grant_next(1, "2026-07-31T00:00:00.000Z", None)
                .await
                .expect("grant_next must not error")
        });
    }
    let mut granted = Vec::new();
    while let Some(result) = attempts.join_next().await {
        if let GrantNextBuildLeaseResult::Granted(row) = result.expect("grant task panicked") {
            granted.push(row);
        }
    }

    let mut authorized = 0i64;
    let mut bound_uid = String::new();
    let mut bound_token = None;
    for row in &granted {
        let uid = row
            .immutable_identity
            .strip_prefix("pod:")
            .expect("the identity carries its Pod UID")
            .to_owned();
        let token = row.fencing_token.expect("a granted lease carries a token");
        let bound = repository
            .bind(&invocation_key(&uid), token, &uid, None)
            .await
            .unwrap_or_else(|e| panic!("bind the lease for {uid}: {e:?}"));
        assert_eq!(bound.state, BuildLeaseState::Bound);
        assert_eq!(bound.bound_pod_uid.as_deref(), Some(uid.as_str()));
        authorized += 1;
        bound_uid = uid;
        bound_token = Some(token);
    }

    // The fence: the SAME lease, presented with the OTHER live Pod's UID.
    let (fence_refused_the_other_uid, binding_after_refusal) = match bound_token {
        Some(token) => {
            let other = uids
                .iter()
                .find(|uid| *uid != &bound_uid)
                .expect("two distinct UIDs");
            let key = invocation_key(&bound_uid);
            let refused = repository
                .bind(&key, token, other, None)
                .await
                .err()
                .map(|error| format!("{error:?}"))
                .is_some_and(|error| error.contains("pod UID does not match build lease"));
            let after = repository
                .get(&key)
                .await
                .expect("re-read the lease row")
                .expect("the lease row exists");
            (refused, after.bound_pod_uid)
        }
        None => (false, None),
    };

    InvocationGovernorOutcome {
        cap,
        queued: uids.len(),
        authorized,
        fence_refused_the_other_uid,
        binding_after_refusal,
        bound_uid,
    }
}

// ===========================================================================
// The live world: a coordinator that can be destroyed and rebuilt
// ===========================================================================

/// Everything a restart interleaving needs, and the ability to lose all of the
/// in-process half of it.
struct LiveWorld {
    context: String,
    db: Database,
    config: KubernetesConfig,
    tokio: tokio::runtime::Runtime,
    task_run_id: String,
    job_name: String,
    /// Every Job this world created, so `cleanup` can delete them all even
    /// after a deliberately duplicated dispatch.
    created_jobs: Vec<String>,
}

impl LiveWorld {
    /// `None` when the live half is disabled; callers `return` early.
    fn open() -> Option<Self> {
        if !live_tests_enabled() {
            return None;
        }
        install_crypto_provider();
        let context = restart_context();
        clear_task_run_jobs(&context);
        set_stop_policy(&context, "None");
        let repair = repair_cluster_queue_for_admission(&context);
        eprintln!("chart repair needed: {}", repair.needed());

        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("build a tokio runtime for the live coordinator");

        let (image_tag, image_digest) = ensure_worker_stub_image(&context);
        let db = Database::open_in_memory().expect("real Postgres test database");
        tokio.block_on(seed_dispatchable_project(&db, &image_tag, &image_digest));

        let config = restart_live_config(&context);
        let task_run_id = uuid::Uuid::now_v7().to_string();
        let job_name = format!("djinn-taskrun-{task_run_id}");
        Some(Self {
            context,
            db,
            config,
            tokio,
            task_run_id,
            job_name,
            created_jobs: Vec::new(),
        })
    }

    /// One coordinator lifetime: build a runtime from a FRESH client and
    /// registry, dispatch the durable spec through the real `prepare`, and hand
    /// back the handle.
    fn dispatch(&mut self) -> RunHandle {
        let context = self.context.clone();
        let config = self.config.clone();
        let db = self.db.clone();
        let spec = self.spec();
        let handle = self.tokio.block_on(async move {
            let runtime = KubernetesRuntime::from_client_with_db(
                kube_client(&context).await,
                config,
                Arc::new(ConnectionRegistry::new()),
                db,
            );
            runtime
                .prepare(&spec, &ResolvedCredentials::default())
                .await
                .expect("the coordinator dispatches the task-run")
            // `runtime` is dropped HERE — the coordinator process ends with it.
        });
        if !self.created_jobs.contains(&self.job_name) {
            self.created_jobs.push(self.job_name.clone());
        }
        handle
    }

    /// The kill. Everything the dead coordinator knew that was not durable goes
    /// with it — most importantly the handle, which is what a task-state write
    /// would have persisted.
    fn restart(&self, handle: RunHandle) {
        eprintln!(
            "COORDINATOR KILLED: dropping the RunHandle for task-run {} (pod_ref={:?}) without \
             persisting anything",
            self.task_run_id, handle.pod_ref,
        );
        drop(handle);
    }

    /// The mutation: a coordinator that came back WITHOUT its durable task-run
    /// id, so nothing it dispatches can collide with what it already created.
    fn remint_task_run_id(&mut self) {
        self.task_run_id = uuid::Uuid::now_v7().to_string();
        self.job_name = format!("djinn-taskrun-{}", self.task_run_id);
        eprintln!(
            "MUTATION: the restarted coordinator lost its durable id and will dispatch {} instead",
            self.job_name,
        );
    }

    fn spec(&self) -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: self.task_run_id.clone(),
            task_attempt_id: None,
            task_id: "fbiy-b2b-task".into(),
            project_id: PROJECT_ID.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "task/fbiy-b2b".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: Vec::new(),
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }

    /// Poll until this run has a `Running` Pod, returning `(name, uid)`.
    fn await_running_pod(&self) -> (String, String) {
        for _ in 0..AWAIT_TICKS {
            if let Some((name, uid, _)) = pods_of(&self.context, &self.task_run_id)
                .into_iter()
                .find(|(_, uid, phase)| !uid.is_empty() && phase == "Running")
            {
                return (name, uid);
            }
            std::thread::sleep(TICK);
        }
        panic!(
            "no Running Pod appeared for task-run {}; workloads: {}; pods: {:?}",
            self.task_run_id,
            workload_summary(&self.context),
            pods_of(&self.context, &self.task_run_id),
        );
    }

    /// Poll until Kueue has CREATED the Workload for this Job, returning its
    /// name. Distinct from waiting for admission: interleaving (a) needs the
    /// object to exist so it can assert the object is not admitted.
    fn await_workload_for_job(&self) -> String {
        for _ in 0..AWAIT_TICKS {
            if let Some(name) = namespace_workloads(&self.context)
                .iter()
                .find(|workload| workload_owner_is(workload, &self.job_name))
                .and_then(|workload| workload["metadata"]["name"].as_str())
            {
                return name.to_owned();
            }
            std::thread::sleep(TICK);
        }
        panic!(
            "Kueue never created a Workload for {}; workloads: {}",
            self.job_name,
            workload_summary(&self.context),
        );
    }

    fn cleanup(&self) {
        for job in &self.created_jobs {
            delete_job(&self.context, job);
        }
        set_stop_policy(&self.context, "None");
    }
}

/// The context every live call is pinned to, after TWO independent refusals of
/// anything else.
///
/// Guard 1 is the name. Guard 2 is the resolved API-server URL, which catches
/// what guard 1 cannot: a kubeconfig entry NAMED `kind-djinn-kueue-b2b` that
/// points somewhere else. kind always serves on loopback; no managed control
/// plane does, and every context in a Djinn developer's kubeconfig is EKS.
fn restart_context() -> String {
    let requested =
        env::var("DJINN_TEST_KUEUE_B2B_CONTEXT").unwrap_or_else(|_| RESTART_CONTEXT.into());
    assert_eq!(
        requested, RESTART_CONTEXT,
        "this harness only ever targets the context of the cluster it creates and deletes",
    );
    let server = kubectl_raw(
        &requested,
        &[
            "config",
            "view",
            "--minify",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ],
    );
    assert!(
        server.starts_with("https://127.0.0.1:")
            || server.starts_with("https://localhost:")
            || server.starts_with("https://[::1]:"),
        "refusing to run against {server}: context {requested} does not resolve to a local kind \
         API server, so it is not a cluster this harness created",
    );
    requested
}

/// The armed `KubernetesConfig` this file renders with.
///
/// `cgroup_launcher_mode: Disabled` mirrors the values fixture: the renderer
/// PANICS if a required launcher is rendered without the RuntimeClass, and this
/// cluster deliberately has none (`fbiy-C1` owns installing it). The requests
/// are lowered from the production defaults so a single kind node can hold the
/// Pod at all.
fn restart_harness_config(image: &str) -> KubernetesConfig {
    KubernetesConfig {
        namespace: NAMESPACE.into(),
        kueue_armed: true,
        kueue_local_queue_prefix: "djinn".into(),
        cgroup_launcher_mode: djinn_k8s::launcher::CgroupLauncherMode::Disabled,
        task_run_cgroup_writable_enabled: false,
        image: image.into(),
        image_pull_policy: "IfNotPresent".into(),
        cpu_request: "100m".into(),
        cpu_limit: "500m".into(),
        memory_request: "64Mi".into(),
        memory_limit: "256Mi".into(),
        ..KubernetesConfig::for_testing()
    }
}

/// The same config, with the ServiceAccount and PVC names the CHART actually
/// created read off the live cluster.
///
/// `KubernetesConfig::for_testing()` carries the unprefixed development names
/// while the chart renders `djinn.fullname` ones. A Pod referencing a
/// nonexistent ServiceAccount or PVC never leaves `Pending`, and every
/// convergence measurement here would then be a measurement of that mistake.
fn restart_live_config(context: &str) -> KubernetesConfig {
    let named = |kind: &str, suffix: &str| -> String {
        kubectl_json(context, &["-n", NAMESPACE, "get", kind])["items"]
            .as_array()
            .expect("a List has items")
            .iter()
            .filter_map(|item| item["metadata"]["name"].as_str())
            .find(|name| name.ends_with(suffix))
            .unwrap_or_else(|| panic!("the chart installs a {kind} ending in {suffix}"))
            .to_owned()
    };
    KubernetesConfig {
        service_account: named("serviceaccounts", "-taskrun"),
        mirror_pvc: named("persistentvolumeclaims", "-mirrors"),
        cache_pvc: named("persistentvolumeclaims", "-cache"),
        ..restart_harness_config("replaced-by-the-catalog-image")
    }
}

/// The absolute path the rendered task-run container actually executes.
fn rendered_worker_command_path(config: &KubernetesConfig) -> String {
    let (job, _) = rendered_task_run_job(config);
    job.spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .map(|pod| &pod.containers)
        .and_then(|containers| containers.first())
        .and_then(|container| container.command.as_ref())
        .and_then(|command| command.first())
        .cloned()
        .expect("the renderer invokes the worker binary explicitly")
}

fn stub_dockerfile(worker_bin: &str) -> String {
    let dir = worker_bin
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("/");
    format!(
        "FROM busybox:1.36\n\
         RUN mkdir -p {dir} \\\n \
         && printf '#!/bin/sh\\nexec sleep 100000\\n' > {worker_bin} \\\n \
         && chmod 0755 {worker_bin}\n"
    )
}

/// Build and push a worker image that EXISTS, runs as uid 1000 and does
/// nothing, and return `(tag, digest)`.
///
/// The real worker binary is not in play: this file measures Job/Workload/Pod
/// identity across a restart, all of which are properties of the objects rather
/// than of what runs inside them, and a real worker would exit non-zero against
/// this cluster's absent server within seconds — turning every convergence
/// measurement into a measurement of that crash.
///
/// Pushed to this harness's registry rather than `kind load`ed because the
/// dispatch path resolves a DIGEST-pinned pull ref (`vf7a` fences images that
/// declare a launcher protocol without an immutable digest), and only a real
/// push mints a manifest digest. The digest is read back from the daemon rather
/// than parsed out of the push transcript.
fn ensure_worker_stub_image(context: &str) -> (String, String) {
    let worker_bin = rendered_worker_command_path(&restart_harness_config("unused"));
    let dir = env::temp_dir().join(format!("djinn-b2b-stub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the stub build context");
    std::fs::write(dir.join("Dockerfile"), stub_dockerfile(&worker_bin))
        .expect("write the stub Dockerfile");

    let tag = format!("localhost:{RESTART_REGISTRY_PORT}/{STUB_IMAGE_REPO}:1");
    for (step, args) in [
        ("build", vec!["build", "-t", &tag, dir.to_str().expect("utf-8 temp dir")]),
        ("push", vec!["push", &tag]),
    ] {
        let output = Command::new("docker")
            .args(&args)
            .output()
            .expect("docker is on PATH");
        assert!(
            output.status.success(),
            "docker {step} of the stub worker image failed: {}",
            stderr(&output),
        );
    }

    let inspected = Command::new("docker")
        .args([
            "image",
            "inspect",
            "-f",
            "{{range .RepoDigests}}{{println .}}{{end}}",
            &tag,
        ])
        .output()
        .expect("docker is on PATH");
    assert!(
        inspected.status.success(),
        "docker image inspect failed: {}",
        stderr(&inspected),
    );
    let prefix = format!("localhost:{RESTART_REGISTRY_PORT}/{STUB_IMAGE_REPO}@");
    let digest = stdout(&inspected)
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix).map(ToOwned::to_owned))
        .unwrap_or_else(|| {
            panic!(
                "the pushed stub image has no repo digest for {prefix}; docker reported: {}",
                stdout(&inspected)
            )
        });
    assert!(
        digest.starts_with("sha256:") && digest.len() == 71,
        "the registry must mint a canonical manifest digest; got {digest}",
    );
    eprintln!("stub worker image: {tag}@{digest} (installs {worker_bin}) on {context}");
    (tag, digest)
}

/// Seed the durable rows the dispatch path reads BEFORE any Kubernetes object
/// is created: a project, and a catalog image that is ready, digest-pinned and
/// declares its launcher authority protocol.
///
/// All three are load-bearing. `resolve_dispatch_image` hard-fails a project
/// with no ready catalog image; `vf7a`'s fence refuses an image that declares a
/// protocol without an immutable digest; and `render_authority_protocol`
/// refuses an image that declares neither. This goes through the real
/// repositories so the fence is the production one.
async fn seed_dispatchable_project(db: &Database, image_tag: &str, image_digest: &str) {
    db.ensure_initialized().await.expect("initialize the database");
    ProjectRepository::new(db.clone(), EventBus::noop())
        .create_with_id(PROJECT_ID, "fbiy-b2b", "djinn-test", PROJECT_ID)
        .await
        .expect("seed the dispatching project");
    let images = ImageRepository::new(db.clone());
    images
        .create(IMAGE_ID, "fbiy-b2b-stub", None, "{}")
        .await
        .expect("seed the catalog image");
    images
        .set_project_image(PROJECT_ID, Some(IMAGE_ID))
        .await
        .expect("assign the catalog image");
    images
        .mark_ready(
            IMAGE_ID,
            image_tag,
            Some(image_digest),
            Some(LauncherAuthorityProtocol::LeafV1),
        )
        .await
        .expect("mark the catalog image ready");

    let resolved = ProjectRepository::new(db.clone(), EventBus::noop())
        .resolve_dispatch_image(PROJECT_ID)
        .await
        .expect("the seeded project resolves a dispatch image")
        .expect("the seeded project has a dispatch image");
    assert_eq!(
        resolved.pull_ref().as_deref(),
        Some(format!("localhost:{RESTART_REGISTRY_PORT}/{STUB_IMAGE_REPO}@{image_digest}").as_str()),
        "the dispatch path must resolve the digest-pinned stub, or the Pod pulls something else",
    );
}

/// Belt and braces on the shared module's tool check: this file also shells out
/// to `docker` for the stub image, so a run without it must skip rather than
/// fail halfway through creating cluster objects.
#[test]
fn guard_the_live_gate_requires_the_tools_this_file_shells_out_to() {
    if env::var("DJINN_TEST_KUEUE_CLUSTER").is_err() {
        assert!(
            !live_tests_enabled(),
            "the live half must stay disabled without DJINN_TEST_KUEUE_CLUSTER=1",
        );
        return;
    }
    for tool in ["kubectl", "docker", "kind"] {
        assert!(
            which(tool) || !live_tests_enabled(),
            "the live gate must not open without {tool} on PATH",
        );
    }
}

/// Recorded, never asserted: how many Workloads the live ClusterQueue was
/// holding when the suite finished. Kept as a `#[test]` rather than an
/// `eprintln!` inside another test so it appears whether or not the
/// convergence tests got that far.
#[test]
#[ignore]
fn live_report_the_final_cluster_queue_census() {
    if !live_tests_enabled() {
        return;
    }
    let context = restart_context();
    eprintln!(
        "FINAL CENSUS: admittedWorkloads={} usage={:?} workloads=[{}]",
        admitted_workloads(&context),
        cluster_queue_usage(&context),
        workload_summary(&context),
    );
}
