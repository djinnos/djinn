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
) -> Result<String, tokio::time::error::Elapsed>
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
        reason.contains(&destroyed_uid) && reason.contains(&job_name),
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
        reason.contains(&format!("worker Pod {destroyed_uid} was deleted")),
        "the run stays bound to the Pod UID it launched, not the replacement; got {reason}"
    );
    assert!(
        reason.contains(&format!(
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
        reason.contains("worker Pod and Job disappeared"),
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
        reason.contains("BackoffLimitExceeded"),
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
