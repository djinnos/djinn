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
//! are re-declared in `tests/kueue_restart/mod.rs` against this file's own
//! triple. That sibling module holds everything that only SERVES these tests —
//! the destroyable coordinator, the stub-image build, the durable seeding and
//! the quota arithmetic — because the pair exceeded
//! `scripts/check-file-size.sh`. Every `#[test]` in `fbiy-B2b` is here.
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

use std::env;

use djinn_db::{Database, InvocationLeaseMode};
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};

mod kueue_disruption;
mod kueue_restart;

use kueue_disruption::{
    VALUES_FIXTURE, delete_job, evict_and_release, exit_code, job_exists, live_tests_enabled,
    pods_of, rendered_task_run_job, run_setup_script, set_stop_policy, stderr, stdout, which,
    workload_summary, yaml_at,
};
use kueue_restart::*;

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
        job.metadata
            .name
            .clone()
            .expect("the renderer names the Job")
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
    let path =
        rendered_worker_command_path(&restart_harness_config("registry.invalid/never-pulled:1"));
    assert!(
        path.starts_with('/'),
        "the rendered worker command must be an absolute path for the stub Dockerfile to \
         install it; got {path}",
    );
    let dockerfile = stub_dockerfile(&path);
    assert!(
        dockerfile.contains(&format!("> {path}"))
            && dockerfile.contains(&format!("chmod 0755 {path}")),
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
    eprintln!(
        "AC4(a) first dispatch: job={} handle={:?}",
        world.job_name, first.pod_ref
    );

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
    let outcome = world.tokio.block_on(drive_invocation_governor(
        &world.db,
        &replacement_uid,
        &admitted_uid,
    ));
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
    assert_ne!(
        original_uid, second_uid,
        "the mutation must produce a SECOND Pod"
    );
    assert!(
        job_exists(&world.context, &original_job) && job_exists(&world.context, &world.job_name),
        "the mutation must leave BOTH Jobs on the API server",
    );

    let context = world.context.clone();
    let job_name = world.job_name.clone();
    for (label, task_run_id, job) in [
        (
            "the original dispatch",
            original_task_run,
            original_job.clone(),
        ),
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
