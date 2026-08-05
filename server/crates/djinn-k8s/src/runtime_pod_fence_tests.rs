//! Containment of a force-deleted task-run Pod (task `buyp`, epic `fbiy`).
//!
//! Three behaviours are under test here, none of which existed before this
//! module:
//!
//! 1. **Detect.** A task-run whose fenced Pod disappears while its Job is still
//!    nonterminal resolves [`KubernetesRuntime::watch_infra_death`], which is
//!    how the dispatch runner terminalises a run
//!    (`djinn-agent/src/actors/slot/supervisor_runner.rs`, the
//!    `watch_infra_death` arm of the `select!` that races the report stream).
//!    Before this, the Pod-gone-Job-alive case fell through the watch by
//!    design; the run held its slot until an unrelated stall reaper collected
//!    it, and the Job held its quota indefinitely.
//! 2. **Reap.** That detection foreground-deletes the owning Job.
//! 3. **Refuse.** The replacement Pod the Job controller mints after a force
//!    delete is observed but never adopted, because the watch is fenced to one
//!    immutable `metadata.uid`.
//!
//! Everything Kubernetes-facing runs against [`FakeCluster`], the stateful
//! in-process apiserver from `runtime_kueue_create_tests`, extended so its
//! modelled Job controller replaces a destroyed Pod with a fresh-UID one. That
//! is real Job-controller behaviour, not Kueue behaviour, which is what makes
//! it an honest fixture — but it is a *model*, and it does not substitute for
//! the live-cluster proof, which is `fbiy-B2`'s job.
//!
//! The build-lease criterion runs against a real PostgreSQL database
//! ([`Database::open_in_memory`] clones the migrated test template), not a mock
//! repository: the rejection under test is a locked transaction plus a
//! trigger-enforced immutable column, and a mock proves neither.

use std::sync::Arc;
use std::time::Duration;

use djinn_db::error::DbError;
use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState,
    GrantNextBuildLeaseResult, QueueBuildLeaseInput,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde_json::Value;

use super::runtime_kueue_create_tests::{ApiCall, FakeCluster};
use super::*;
use crate::config::KubernetesConfig;
use crate::secret::task_run_resource_name;

const FENCE_TASK_RUN_ID: &str = "019f72b5-a92a-7501-8b41-b0ffe68cdda5";

/// Virtual-time budget for a watch that is *expected* to resolve. Time is
/// paused in these tests, so this costs no wall clock — it exists so a watch
/// that never resolves fails as a timeout instead of hanging the suite.
const WATCH_BUDGET: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fence_config() -> KubernetesConfig {
    let mut config = KubernetesConfig::for_testing();
    // Disarmed: the Job is rendered without `suspend`, so the modelled Job
    // controller pods it immediately and there is a Pod to fence on. Kueue
    // admission is orthogonal to this containment and has its own coverage.
    config.kueue_armed = false;
    config
}

/// Armed: the Job renders `suspend: true` plus the `queue-name` label, so the
/// fake captures a Kueue `Workload` for it and there is an admission — and an
/// EVICTION — to model at all. `03z3`'s subject.
fn armed_fence_config() -> KubernetesConfig {
    let mut config = KubernetesConfig::for_testing();
    config.kueue_armed = true;
    config
}

fn runtime_on(cluster: &Arc<FakeCluster>, config: KubernetesConfig) -> KubernetesRuntime {
    KubernetesRuntime {
        client: cluster.client(),
        config,
        registry: Arc::new(ConnectionRegistry::new()),
        // `watch_infra_death` never touches the database; a `Some` here would
        // only add a Postgres round-trip to a test about Kubernetes.
        db: None,
        read_source_preparation: None,
        dispatch_image_override: Some("registry/test:test".into()),
        pending: Arc::new(Mutex::new(HashMap::new())),
    }
}

fn run_handle(job_name: &str) -> RunHandle {
    RunHandle {
        task_run_id: FENCE_TASK_RUN_ID.into(),
        container_id: None,
        pod_ref: Some(job_name.to_string()),
        started_at: SystemClock::new().now(),
        job_uid: None,
        launcher_authority_protocol: None,
    }
}

/// Create the run's Job through the fake apiserver using the *real* manifest
/// builder, so the label the watch selects on is the label production writes.
async fn seed_running_taskrun(cluster: &Arc<FakeCluster>, config: &KubernetesConfig) -> String {
    let task_run_id: Uuid = FENCE_TASK_RUN_ID.parse().expect("task-run uuid");
    let job = crate::job::build_task_run_job_with_read_sources(
        config,
        &task_run_id,
        "owner-project-id",
        &task_run_resource_name(&task_run_id),
        "registry/test:test",
        &[],
        None,
        false,
        None,
        None,
    );
    let jobs: Api<Job> = Api::namespaced(cluster.client(), &config.namespace);
    jobs.create(&PostParams::default(), &job)
        .await
        .expect("seed the task-run Job")
        .metadata
        .name
        .expect("the seeded Job is named")
}

fn pod_list_calls(cluster: &Arc<FakeCluster>) -> usize {
    cluster
        .calls()
        .iter()
        .filter(|call| call.method == "GET" && call.path.ends_with("/pods"))
        .count()
}

/// Let the spawned watch run until it has listed Pods at least once — i.e.
/// until it has bound its fence to the Pod that exists *now*.
///
/// Time is paused, so this cannot busy-wait forever on the clock: the loop
/// keeps the runtime busy, which is precisely what stops the watch's 15s poll
/// sleep from auto-advancing before we have mutated the cluster.
async fn await_fence_binding(cluster: &Arc<FakeCluster>) {
    for _ in 0..10_000 {
        if pod_list_calls(cluster) >= 1 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the infra-death watch never listed Pods, so it never bound its fence");
}

/// Start the watch, let it bind its fence to the live Pod, then apply `mutate`
/// to the cluster and await the watch's resolution.
///
/// The ordering is the whole point: a watch that mutates *before* observing has
/// nothing to be fenced to, and would pass trivially.
async fn watch_after_fence_binding<F>(
    cluster: &Arc<FakeCluster>,
    runtime: KubernetesRuntime,
    job_name: &str,
    mutate: F,
) -> Result<TerminalRuntimeObservation, tokio::time::error::Elapsed>
where
    F: FnOnce(&Arc<FakeCluster>),
{
    let handle = run_handle(job_name);
    let watch = tokio::spawn(async move { runtime.watch_infra_death(&handle).await });
    await_fence_binding(cluster).await;
    mutate(cluster);
    tokio::time::timeout(WATCH_BUDGET, watch)
        .await
        .map(|joined| joined.expect("the infra-death watch task panicked"))
}

/// Let the running watch take `polls` more turns of its loop.
///
/// Time is paused, so this costs no wall clock. Each iteration sleeps slightly
/// PAST [`INFRA_DEATH_POLL_INTERVAL`]: with the test task parked the runtime
/// goes idle and tokio auto-advances the clock to the watch's own deadline
/// first, which is the only way a paused test can make the watch observe
/// anything after its initial binding poll. Yielding instead would keep a task
/// runnable forever, the clock would never advance, and the watch would sit at
/// its sleep for the whole test — passing every "did not resolve" assertion
/// without ever having looked at the cluster.
async fn let_the_watch_poll(cluster: &Arc<FakeCluster>, polls: usize) {
    let before = pod_list_calls(cluster);
    for _ in 0..polls {
        tokio::time::sleep(INFRA_DEATH_POLL_INTERVAL + Duration::from_secs(1)).await;
    }
    assert!(
        pod_list_calls(cluster) >= before + polls,
        "the watch must have polled {polls} more times ({before} -> {}); a test that asserts \
         'it did not resolve' while the watch is parked asserts nothing",
        pod_list_calls(cluster),
    );
}

/// Every DELETE the fake apiserver observed against `job_name`, with the body
/// the client actually sent. Kubernetes carries delete options in the DELETE
/// *body*, so this is where `propagationPolicy` is provable.
fn job_delete_calls(cluster: &Arc<FakeCluster>, job_name: &str) -> Vec<ApiCall> {
    cluster
        .calls()
        .into_iter()
        .filter(|call| call.method == "DELETE" && call.path.ends_with(job_name))
        .collect()
}

// ---------------------------------------------------------------------------
// AC1 — detect the disappearance and reap the Job
// ---------------------------------------------------------------------------

/// A force-deleted worker Pod terminalises its run AND leaves no Job behind.
///
/// The load-bearing assertion is `cluster.job_names().is_empty()`: the Job is
/// gone from the fake apiserver's object store. Making the deletion a no-op
/// fails here, on the surviving Job — not on a missing log line, and not on a
/// `DeleteParams` value that was constructed and never sent.
///
/// Both halves are asserted because either alone is satisfiable by a bug. A
/// watch that resolves without reaping leaves the Job holding Kueue quota for a
/// run nobody is waiting on; a reap that never resolves the watch leaves the
/// dispatch slot pinned until an unrelated stall reaper collects it.
#[tokio::test(start_paused = true)]
async fn a_force_deleted_worker_pod_terminalises_the_run_and_reaps_its_job() {
    let cluster = FakeCluster::new();
    let config = fence_config();
    let job_name = seed_running_taskrun(&cluster, &config).await;
    assert_eq!(
        cluster.pod_count(),
        1,
        "the seeded task-run Job must have materialised exactly one worker Pod"
    );
    let runtime = runtime_on(&cluster, config);

    let mut destroyed_uid = String::new();
    let reason = watch_after_fence_binding(&cluster, runtime, &job_name, |cluster| {
        let (destroyed, replacement) = cluster.force_delete_pod_of(&job_name);
        assert!(
            replacement.is_some_and(|uid| uid != destroyed),
            "the fixture must model the Job controller replacing the destroyed Pod with a \
             fresh-UID one — without that there is nothing to refuse to adopt"
        );
        destroyed_uid = destroyed;
    })
    .await
    .expect("a force-deleted worker Pod under a live Job must resolve the infra-death watch");

    assert!(
        reason.diagnostic.contains(&destroyed_uid) && reason.diagnostic.contains(&job_name),
        "the death reason must name the destroyed Pod and its Job; got {reason}"
    );
    assert!(
        cluster.job_names().is_empty(),
        "the orphaned task-run Job must be deleted, so it stops holding quota; still present: {:?}",
        cluster.job_names()
    );
}

// ---------------------------------------------------------------------------
// AC2 — the replacement Pod is never adopted
// ---------------------------------------------------------------------------

/// The run's recorded Pod UID after the replacement is still the ORIGINAL.
///
/// `watch_infra_death` returns the reason the dispatch runner terminalises with,
/// so the UID it names is the run's recorded Pod identity as far as anything
/// downstream can see. Removing the UID comparison from `fenced_worker_pod`
/// (falling back to `pods.first()`) makes the watch adopt the replacement: it
/// sees a healthy Pod, never resolves, and this test fails on the
/// [`WATCH_BUDGET`] timeout.
#[tokio::test(start_paused = true)]
async fn a_replacement_pod_with_a_different_uid_is_observed_but_never_adopted() {
    let cluster = FakeCluster::new();
    let config = fence_config();
    let job_name = seed_running_taskrun(&cluster, &config).await;
    let runtime = runtime_on(&cluster, config);

    let mut destroyed_uid = String::new();
    let mut replacement_uid = String::new();
    let reason = watch_after_fence_binding(&cluster, runtime, &job_name, |cluster| {
        let (destroyed, replacement) = cluster.force_delete_pod_of(&job_name);
        destroyed_uid = destroyed;
        replacement_uid = replacement.expect("the Job controller minted a replacement Pod");
    })
    .await
    .expect(
        "the watch must resolve on the fenced Pod's disappearance; adopting the replacement \
         instead leaves it watching forever",
    );

    assert_ne!(
        destroyed_uid, replacement_uid,
        "fixture invariant: the replacement must carry a different immutable UID"
    );
    assert!(
        reason
            .diagnostic
            .contains(&format!("worker Pod {destroyed_uid} was deleted")),
        "the run stays bound to the Pod UID it launched, not the replacement; got {reason}"
    );
    assert!(
        reason.diagnostic.contains(&format!(
            "refused to adopt replacement Pod UID(s) {replacement_uid}"
        )),
        "the replacement must be reported as refused, not silently ignored; got {reason}"
    );
    assert!(
        cluster.pod_uids().contains(&replacement_uid) || cluster.pod_uids().is_empty(),
        "fixture invariant: the replacement Pod either survived the observation or was \
         cascade-deleted with its Job"
    );
}

/// The pure fence, exercised directly: selection is by immutable UID, and every
/// other Pod under the same labels is reported as unadopted.
#[test]
fn the_pod_fence_selects_by_uid_and_reports_every_other_pod_as_unadopted() {
    fn pod(uid: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(format!("pod-{uid}")),
                uid: Some(uid.to_string()),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        }
    }

    let listed = vec![pod("replacement"), pod("original")];
    assert_eq!(
        fenced_worker_pod(&listed, Some("original"))
            .and_then(|pod| pod.metadata.uid.clone())
            .as_deref(),
        Some("original"),
        "position must never decide identity — the replacement is listed first here"
    );
    assert!(
        fenced_worker_pod(&listed, Some("evicted")).is_none(),
        "a fence whose Pod is absent must report absence, not the nearest neighbour"
    );
    assert_eq!(
        unadopted_pod_uids(&listed, Some("original")),
        vec!["replacement".to_string()]
    );
    assert_eq!(
        bind_worker_pod_uid(&listed).as_deref(),
        Some("replacement"),
        "binding takes the first UID it can see; after that the fence is frozen"
    );
    assert!(bind_worker_pod_uid(&[]).is_none());
}

// ---------------------------------------------------------------------------
// AC4 — the reaping delete is Foreground, on the wire
// ---------------------------------------------------------------------------

/// `propagationPolicy: Foreground` asserted on the request body the fake
/// apiserver received.
///
/// Kubernetes carries delete options in the DELETE body, so this observes what
/// was *sent*. Inspecting the `DeleteParams` value instead would pass for an
/// implementation that builds the params and never issues the request — and for
/// one that issues it against the wrong name.
///
/// Switching the reap to `DeleteParams::background()` fails on the body's
/// `propagationPolicy`.
#[tokio::test(start_paused = true)]
async fn the_reaping_job_delete_carries_foreground_propagation_in_its_request_body() {
    let cluster = FakeCluster::new();
    let config = fence_config();
    let job_name = seed_running_taskrun(&cluster, &config).await;
    let runtime = runtime_on(&cluster, config);

    watch_after_fence_binding(&cluster, runtime, &job_name, |cluster| {
        cluster.force_delete_pod_of(&job_name);
    })
    .await
    .expect("the watch resolves on the fenced Pod's disappearance");

    let deletes = job_delete_calls(&cluster, &job_name);
    assert_eq!(
        deletes.len(),
        1,
        "the reap must issue exactly one Job DELETE; observed {deletes:?}"
    );
    let body = deletes[0]
        .body
        .as_ref()
        .expect("a Kubernetes DELETE carries its options as a JSON body");
    assert_eq!(
        body.get("propagationPolicy").and_then(Value::as_str),
        Some("Foreground"),
        "the reap must block the Job's removal on its Pods being cleaned up; body was {body}"
    );
    assert_eq!(
        body.get("gracePeriodSeconds").and_then(Value::as_i64),
        Some(i64::from(INFRA_DEATH_REAP_GRACE_SECONDS)),
        "the reap must offer the same termination grace as cancel/teardown; body was {body}"
    );
}

// ---------------------------------------------------------------------------
// AC5 — the pre-existing resolution arms are unchanged
// ---------------------------------------------------------------------------

/// Pod-and-Job-both-gone still resolves with its original reason, and the new
/// containment does NOT fire for it (there is no Job left to reap).
#[tokio::test(start_paused = true)]
async fn pod_and_job_both_gone_still_resolves_with_its_original_reason() {
    let cluster = FakeCluster::new();
    let config = fence_config();
    let job_name = seed_running_taskrun(&cluster, &config).await;
    let runtime = runtime_on(&cluster, config);

    let reason = watch_after_fence_binding(&cluster, runtime, &job_name, |cluster| {
        cluster.gc_pod_of(&job_name);
        cluster.gc_job(&job_name);
    })
    .await
    .expect("the pre-existing pod-and-job-both-gone arm must still resolve");

    assert!(
        reason.diagnostic.contains("worker Pod and Job disappeared"),
        "the both-gone arm must keep its own reason; got {reason}"
    );
    assert!(
        job_delete_calls(&cluster, &job_name).is_empty(),
        "nothing to reap when the Job is already gone; observed {:?}",
        job_delete_calls(&cluster, &job_name)
    );
}

/// A `Failed` Job still resolves with its condition reason, on the first poll,
/// with its Pod still present.
#[tokio::test(start_paused = true)]
async fn a_failed_job_still_resolves_with_its_condition_reason() {
    let cluster = FakeCluster::new();
    let config = fence_config();
    let job_name = seed_running_taskrun(&cluster, &config).await;
    cluster.fail_job(&job_name, "BackoffLimitExceeded");
    let runtime = runtime_on(&cluster, config);
    let handle = run_handle(&job_name);

    let reason = tokio::time::timeout(WATCH_BUDGET, runtime.watch_infra_death(&handle))
        .await
        .expect("the pre-existing Job-Failed arm must still resolve");

    assert!(
        reason.diagnostic.contains("BackoffLimitExceeded"),
        "the Job-Failed arm must keep reporting the apiserver's condition reason; got {reason}"
    );
    assert_eq!(
        cluster.job_names(),
        vec![job_name.clone()],
        "the Job-Failed arm reaps nothing — a terminal Job holds no quota, and teardown owns it"
    );
}

/// A cleanly Complete Job whose Pod was TTL-GC'd is NOT a death, and is NOT
/// reaped.
///
/// This is the containment's own blast radius. The new arm fires on *any*
/// fenced-Pod disappearance under a live Job; gating it on the Job being
/// nonterminal is what stops it from racing — and deleting — the Job of a run
/// that finished cleanly and whose terminal report is still on the wire.
#[tokio::test(start_paused = true)]
async fn a_completed_job_whose_pod_was_ttl_gcd_is_neither_a_death_nor_reaped() {
    let cluster = FakeCluster::new();
    let config = fence_config();
    let job_name = seed_running_taskrun(&cluster, &config).await;
    let runtime = runtime_on(&cluster, config);

    let outcome = watch_after_fence_binding(&cluster, runtime, &job_name, |cluster| {
        cluster.complete_job(&job_name);
        cluster.gc_pod_of(&job_name);
    })
    .await;

    assert!(
        outcome.is_err(),
        "a clean completion must not resolve the infra-death watch; it resolved with {outcome:?}"
    );
    assert_eq!(
        cluster.job_names(),
        vec![job_name.clone()],
        "a completed run's Job must survive — its terminal report rides the stream"
    );
    assert!(
        job_delete_calls(&cluster, &job_name).is_empty(),
        "the containment must not reap a cleanly completed Job"
    );
}

// ---------------------------------------------------------------------------
// `03z3` — a Kueue eviction is RECOVERABLE and must not be reaped
// ---------------------------------------------------------------------------

/// Seed an ARMED task-run and drive it to admitted-and-running: the Job is
/// created suspended with a captured Workload, and only the modelled admission
/// gives it a Pod. Returns the Job name.
async fn seed_admitted_taskrun(cluster: &Arc<FakeCluster>, config: &KubernetesConfig) -> String {
    let job_name = seed_running_taskrun(cluster, config).await;
    assert_eq!(
        cluster.pod_count(),
        0,
        "an armed task-run Job is created SUSPENDED, so Kueue owns whether it ever pods",
    );
    cluster.unsuspend(&job_name);
    assert_eq!(
        cluster.pod_count(),
        1,
        "admission must materialise exactly one worker Pod to fence on",
    );
    job_name
}

/// Start the watch and let it bind its fence to the admitted Pod.
fn watch_of(
    runtime: KubernetesRuntime,
    job_name: &str,
) -> tokio::task::JoinHandle<TerminalRuntimeObservation> {
    let handle = run_handle(job_name);
    tokio::spawn(async move { runtime.watch_infra_death(&handle).await })
}

/// AC1. A Kueue eviction followed by a re-admission leaves the run ALIVE.
///
/// The two load-bearing assertions are that the watch has NOT resolved (the
/// dispatch runner terminalises the run the instant it does) and that the Job is
/// still on the API server (a reaped Job can never be re-admitted, and a retry
/// mints a fresh task-run id so nothing ever adopts the old one back).
///
/// Non-vacuity, both directions:
/// * restore the unconditional foreground delete — drop the `classify_absent_pod`
///   match and reap on every absence — and the watch resolves during the
///   eviction, so `is_finished` fails and the Job is gone;
/// * make the watch never poll (it sits parked at its sleep) and
///   [`let_the_watch_poll`] fails instead, so "did not resolve" cannot pass by
///   the watch not having looked.
#[tokio::test(start_paused = true)]
async fn a_kueue_eviction_then_re_admission_leaves_the_task_run_alive() {
    let cluster = FakeCluster::new();
    let config = armed_fence_config();
    let job_name = seed_admitted_taskrun(&cluster, &config).await;
    let watch = watch_of(runtime_on(&cluster, config), &job_name);
    await_fence_binding(&cluster).await;

    let evicted_uid = cluster.evict(&job_name, "ClusterQueueStopped");
    let_the_watch_poll(&cluster, 2).await;
    assert!(
        !watch.is_finished(),
        "an evicted run is recoverable: the watch must not terminalise it, and it resolved. \
         Surviving Jobs: {:?}",
        cluster.job_names(),
    );
    assert_eq!(
        cluster.job_names(),
        vec![job_name.clone()],
        "the evicted Job must survive — deleting it is what makes the eviction unrecoverable",
    );

    let readmitted_uid = cluster.readmit(&job_name);
    assert_ne!(
        evicted_uid, readmitted_uid,
        "fixture invariant: re-admission mints a Pod with a NEW immutable uid",
    );
    let_the_watch_poll(&cluster, 3).await;

    assert!(
        !watch.is_finished(),
        "the re-admitted run must still be alive: its new Pod is running and its terminal report \
         will ride the stream",
    );
    assert_eq!(cluster.job_names(), vec![job_name.clone()]);
    assert!(
        job_delete_calls(&cluster, &job_name).is_empty(),
        "no Job DELETE may be issued across an eviction; observed {:?}",
        job_delete_calls(&cluster, &job_name),
    );
    watch.abort();
}

/// AC3, and the sample that actually happens on a cluster.
///
/// Measured live on 2026-07-31: after the queue is released, `spec.suspend` is
/// back to `false` and a new Pod exists within ~1 SECOND, against a 15 second
/// poll. So the poll that matters usually lands *after* re-admission, where the
/// Job is unsuspended and the fenced Pod is gone — bit-for-bit the state fbiy-A1
/// reaps on. Here the watch never observes the suspension at all.
///
/// What holds the reap off is the Workload's `Evicted` condition, which Kueue
/// leaves behind flipped to `False`. Make
/// [`crate::runtime_eviction::workload_eviction_record`] status-sensitive — read
/// only `Evicted == True`, as `classify_workload_admission` correctly does for a
/// different question — and this test fails while the previous one still passes.
#[tokio::test(start_paused = true)]
async fn a_re_admission_the_watch_never_saw_suspended_still_leaves_the_run_alive() {
    let cluster = FakeCluster::new();
    let config = armed_fence_config();
    let job_name = seed_admitted_taskrun(&cluster, &config).await;
    let watch = watch_of(runtime_on(&cluster, config), &job_name);
    await_fence_binding(&cluster).await;

    // Both transitions between two polls: the watch sees only the end state.
    let evicted_uid = cluster.evict(&job_name, "ClusterQueueStopped");
    let readmitted_uid = cluster.readmit(&job_name);
    assert_ne!(evicted_uid, readmitted_uid);
    assert_eq!(
        cluster
            .job(&job_name)
            .and_then(|job| job.pointer("/spec/suspend").and_then(Value::as_bool)),
        Some(false),
        "the case under test is the one where `spec.suspend` has ALREADY gone back to false, so \
         the Job-level signal is exhausted before the watch ever looks",
    );

    let_the_watch_poll(&cluster, 3).await;

    assert!(
        !watch.is_finished(),
        "the only evidence left is Kueue's own Evicted record, and it must be enough",
    );
    assert_eq!(cluster.job_names(), vec![job_name.clone()]);
    assert!(job_delete_calls(&cluster, &job_name).is_empty());
    watch.abort();
}

/// The Job-level half of the distinguisher, with no Workload in existence.
///
/// `spec.suspend` is not merely a faster path to the same answer: it is the only
/// answer available when the Workload cannot be read (a cluster with no Kueue,
/// an operator's own `kubectl patch suspend=true`, an RBAC gap). A suspended Job
/// is nonterminal but NOT owed a Pod, so its missing Pod is not evidence of
/// anything.
///
/// Non-vacuity: drop the `job_is_suspended` arm from `classify_absent_pod` and
/// this test fails — there is no Workload here to fall back on, because the
/// disarmed renderer stamps no `queue-name` label and the fake captures nothing.
#[tokio::test(start_paused = true)]
async fn a_suspended_job_with_no_workload_at_all_is_not_reaped() {
    let cluster = FakeCluster::new();
    let config = fence_config();
    let job_name = seed_running_taskrun(&cluster, &config).await;
    let watch = watch_of(runtime_on(&cluster, config), &job_name);
    await_fence_binding(&cluster).await;

    cluster.evict(&job_name, "ClusterQueueStopped");
    assert!(
        cluster.workload_names().is_empty(),
        "fixture invariant: a disarmed Job carries no queue-name label, so Kueue captures no \
         Workload and `spec.suspend` is the only evidence there is",
    );

    let_the_watch_poll(&cluster, 3).await;
    assert!(!watch.is_finished(), "a suspended Job is not owed a Pod");
    assert_eq!(cluster.job_names(), vec![job_name.clone()]);
    assert!(job_delete_calls(&cluster, &job_name).is_empty());
    watch.abort();
}

/// AC2 under Kueue. The narrowing must not disarm the containment for a Job
/// whose Workload was never evicted.
///
/// Same armed topology as the eviction tests — a captured Workload, a real
/// admission — but the Pod is force-deleted rather than evicted, so the Workload
/// carries no `Evicted` condition and `spec.suspend` is false. That is an
/// UNEXPLAINED absence, and it must still terminalise and reap.
///
/// Non-vacuity: hold on every absence (return `Recoverable` unconditionally from
/// `classify_absent_pod`) and this test fails on the surviving Job, while
/// `a_force_deleted_worker_pod_terminalises_the_run_and_reaps_its_job` covers
/// the same claim with no Kueue in the picture at all.
#[tokio::test(start_paused = true)]
async fn a_force_delete_under_a_never_evicted_workload_still_reaps() {
    let cluster = FakeCluster::new();
    let config = armed_fence_config();
    let job_name = seed_admitted_taskrun(&cluster, &config).await;
    let runtime = runtime_on(&cluster, config);

    let mut destroyed_uid = String::new();
    let reason = watch_after_fence_binding(&cluster, runtime, &job_name, |cluster| {
        let (destroyed, replacement) = cluster.force_delete_pod_of(&job_name);
        assert!(
            replacement.is_some_and(|uid| uid != destroyed),
            "fixture invariant: an UNSUSPENDED Job's controller replaces a destroyed Pod",
        );
        destroyed_uid = destroyed;
    })
    .await
    .expect(
        "a force-deleted Pod under an admitted, never-evicted Workload is still an abandoned run",
    );

    assert!(
        reason.diagnostic.contains(&destroyed_uid),
        "the death reason must name the destroyed Pod; got {reason}",
    );
    assert!(
        cluster.job_names().is_empty(),
        "the abandoned Job must still be reaped, or it holds its Kueue quota forever; still \
         present: {:?}",
        cluster.job_names(),
    );
}

// ---------------------------------------------------------------------------
// AC3 — the build lease refuses the replacement UID, in real PostgreSQL
// ---------------------------------------------------------------------------

fn invocation_key() -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: format!("invocation-{FENCE_TASK_RUN_ID}"),
    }
}

fn queue_input() -> QueueBuildLeaseInput {
    QueueBuildLeaseInput {
        key: invocation_key(),
        immutable_identity: format!("task:{FENCE_TASK_RUN_ID}"),
        queue_deadline: None,
        launch_deadline: None,
        weight: 1,
    }
}

fn assert_pod_uid_mismatch(error: DbError, context: &str) {
    match error {
        DbError::InvalidTransition(message) => assert_eq!(
            message, "pod UID does not match build lease",
            "{context}: rejected for the wrong reason"
        ),
        other => panic!("{context}: expected an InvalidTransition rejection, got {other:?}"),
    }
}

/// Every lift presented with the replacement Pod's UID is rejected by
/// PostgreSQL, from every occupied lifecycle state, and the durable row keeps
/// the original binding.
///
/// Run against a real database rather than a mock repository on purpose: the
/// rejection is a locked read-modify-write inside a transaction plus a
/// trigger-enforced immutable column. A mock repository asserts only that the
/// Rust `if` was written — it cannot show that a concurrent lift loses, and it
/// cannot show the column is immutable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_lift_presented_with_the_replacement_pod_uid_is_rejected() {
    let db = Database::open_in_memory().expect("real Postgres test database");
    let repository = BuildLeaseRepository::new(db);
    let key = invocation_key();

    repository
        .queue(&queue_input())
        .await
        .expect("queue the invocation lease");
    let granted = repository
        .grant_next(1, "2026-07-30T00:00:00.000Z", None)
        .await
        .expect("grant the queued lease");
    let token = match granted {
        GrantNextBuildLeaseResult::Granted(row) => {
            row.fencing_token.expect("a granted lease carries a token")
        }
        other => panic!("expected a grant, got {other:?}"),
    };

    let original = "pod-uid-original";
    let replacement = "pod-uid-replacement";

    let bound = repository
        .bind(&key, token, original, None)
        .await
        .expect("bind the lease to the Pod the run actually launched");
    assert_eq!(bound.state, BuildLeaseState::Bound);
    assert_eq!(bound.bound_pod_uid.as_deref(), Some(original));

    // Every occupied state a live run passes through, each presented with the
    // replacement UID. `bind` is the sole operation that carries a Pod identity,
    // so this is the complete lift surface.
    for state in [
        BuildLeaseState::Bound,
        BuildLeaseState::Active,
        BuildLeaseState::Suspect,
    ] {
        if state != BuildLeaseState::Bound {
            repository
                .status(&key, token, state, None)
                .await
                .unwrap_or_else(|e| panic!("advance the lease to {state:?}: {e:?}"));
        }
        let rejected = repository
            .bind(&key, token, replacement, None)
            .await
            .expect_err(&format!(
                "a lift from {state:?} presenting the replacement Pod UID must be rejected"
            ));
        assert_pod_uid_mismatch(rejected, &format!("lift from {state:?}"));

        // Re-presenting the ORIGINAL identity is still accepted, so the
        // rejection is about identity and not about the state being frozen.
        repository
            .bind(&key, token, original, None)
            .await
            .unwrap_or_else(|e| panic!("replaying the original binding from {state:?}: {e:?}"));
    }

    let row = repository
        .get(&key)
        .await
        .expect("read the durable lease row")
        .expect("the lease row exists");
    assert_eq!(
        row.bound_pod_uid.as_deref(),
        Some(original),
        "no rejected lift may have moved the durable Pod binding"
    );
}

/// `03z3` AC4. Surviving an eviction must not buy recovery with containment.
///
/// The uid here is not a literal: it is the one the modelled re-admission
/// actually minted, so this asserts about the same object the previous tests let
/// the run keep living beside. A run that recovers is still a run whose build
/// lease is bound to the Pod it launched, and the Pod that came back is not that
/// Pod.
///
/// Non-vacuity: allow the new uid — drop the pod-uid comparison from `bind`, or
/// present `readmitted` as the original — and the `expect_err` fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lift_from_the_re_admitted_pod_is_still_rejected() {
    let cluster = FakeCluster::new();
    let config = armed_fence_config();
    let job_name = seed_admitted_taskrun(&cluster, &config).await;
    let original = cluster
        .pod_uids()
        .first()
        .expect("the admitted run has a Pod")
        .clone();
    cluster.evict(&job_name, "ClusterQueueStopped");
    let readmitted = cluster.readmit(&job_name);
    assert_ne!(original, readmitted);

    let db = Database::open_in_memory().expect("real Postgres test database");
    let repository = BuildLeaseRepository::new(db);
    let key = invocation_key();
    repository
        .queue(&queue_input())
        .await
        .expect("queue the invocation lease");
    let token = match repository
        .grant_next(1, "2026-07-30T00:00:00.000Z", None)
        .await
        .expect("grant the queued lease")
    {
        GrantNextBuildLeaseResult::Granted(row) => {
            row.fencing_token.expect("a granted lease carries a token")
        }
        other => panic!("expected a grant, got {other:?}"),
    };
    repository
        .bind(&key, token, &original, None)
        .await
        .expect("bind the lease to the Pod the run launched before the eviction");

    let rejected = repository
        .bind(&key, token, &readmitted, None)
        .await
        .expect_err("the Pod Kueue re-admitted is not the Pod this lease is bound to");
    assert_pod_uid_mismatch(rejected, "lift from the re-admitted Pod");

    let row = repository
        .get(&key)
        .await
        .expect("read the durable lease row")
        .expect("the lease row exists");
    assert_eq!(
        row.bound_pod_uid.as_deref(),
        Some(original.as_str()),
        "recovery must not move the durable Pod binding",
    );
}
