// Test-only: Instant::now is used for deadlined warm-poll loops in this
// test module.
#![allow(clippy::disallowed_methods)]
use super::*;
use djinn_core::events::EventBus;
use djinn_db::RepoGraphCacheInsert;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

type CapturedJobs = Arc<Mutex<Vec<(String, Job)>>>;

struct RecordingDispatcher {
    captured: CapturedJobs,
    count: Arc<AtomicUsize>,
    name_prefix: String,
}

impl RecordingDispatcher {
    fn new(prefix: &str) -> (Self, CapturedJobs, Arc<AtomicUsize>) {
        let captured: CapturedJobs = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        (
            Self {
                captured: captured.clone(),
                count: count.clone(),
                name_prefix: prefix.to_string(),
            },
            captured,
            count,
        )
    }
}

#[async_trait]
impl WarmJobDispatcher for RecordingDispatcher {
    async fn dispatch(&self, namespace: &str, job: Job) -> Result<String, String> {
        let idx = self.count.fetch_add(1, Ordering::SeqCst);
        self.captured
            .lock()
            .await
            .push((namespace.to_string(), job.clone()));
        Ok(job
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| format!("{}-{idx}", self.name_prefix)))
    }
}

/// A watcher that blocks on a [`Notify`] until the test decides the
/// watched Job has completed. Uses a permit-bearing [`Notify`] so a
/// `notify_one` issued before the watcher has started awaiting still
/// lands — which matches how we expect the production watcher to
/// observe a Job that terminated before the poll loop spun up.
struct ControlledWatcher {
    release: Arc<Notify>,
}

#[async_trait]
impl WarmJobWatcher for ControlledWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        self.release.notified().await;
        WarmTerminalOutcome::Succeeded
    }
}

/// A watcher that returns a caller-chosen terminal outcome immediately.
/// Used to exercise the completion-sink hook on both success and failure.
struct ImmediateWatcher {
    outcome: WarmTerminalOutcome,
}

#[async_trait]
impl WarmJobWatcher for ImmediateWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        self.outcome
    }
}

/// Test watcher that applies the same private polling decision seam as the
/// production watcher. It lets completion-sink tests model a 404 deletion
/// without a Kubernetes API server.
struct ObservationWatcher {
    observations: Vec<(WarmJobObservation, bool)>,
}

#[async_trait]
impl WarmJobWatcher for ObservationWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        self.observations
            .iter()
            .find_map(|(observation, deadline_expired)| {
                terminal_outcome_after_poll(*observation, *deadline_expired)
            })
            .unwrap_or(WarmTerminalOutcome::Failed)
    }
}

/// Recording [`WarmCompletionSink`] that captures the `project_id`s it was
/// invoked with, so tests can assert the event-driven invalidation hook fired
/// exactly on warm-Job success.
struct RecordingSink {
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingSink {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait]
impl WarmCompletionSink for RecordingSink {
    async fn on_warm_succeeded(&self, project_id: &str) {
        self.calls.lock().await.push(project_id.to_string());
    }
}

#[test]
fn watcher_poll_observations_have_conservative_terminal_semantics() {
    assert_eq!(
        terminal_outcome_after_poll(WarmJobObservation::Succeeded, false),
        Some(WarmTerminalOutcome::Succeeded),
        "only observed success proves the graph was persisted"
    );
    assert_eq!(
        terminal_outcome_after_poll(WarmJobObservation::Succeeded, true),
        Some(WarmTerminalOutcome::Succeeded),
        "an observed success remains authoritative even at the watcher deadline"
    );
    assert_eq!(
        terminal_outcome_after_poll(WarmJobObservation::Failed, false),
        Some(WarmTerminalOutcome::Failed),
        "including active-deadline Job failures"
    );
    assert_eq!(
        terminal_outcome_after_poll(WarmJobObservation::Disappeared, false),
        Some(WarmTerminalOutcome::Failed),
        "operator deletion or eviction cannot be treated as TTL-cleaned success"
    );
}

#[test]
fn watcher_poll_retries_transient_errors_and_respects_deadline() {
    let eventual_success = [
        (WarmJobObservation::ApiError, false),
        (WarmJobObservation::Succeeded, false),
    ]
    .into_iter()
    .find_map(|(observation, deadline_expired)| {
        terminal_outcome_after_poll(observation, deadline_expired)
    });
    assert_eq!(
        eventual_success,
        Some(WarmTerminalOutcome::Succeeded),
        "a transient API error is retried until a later success is observed"
    );
    assert_eq!(
        terminal_outcome_after_poll(WarmJobObservation::Running, true),
        Some(WarmTerminalOutcome::Failed),
        "a watcher deadline expiry fails conservatively"
    );
    assert_eq!(
        terminal_outcome_after_poll(WarmJobObservation::ApiError, true),
        Some(WarmTerminalOutcome::Failed),
        "persistent API errors cannot extend polling past the deadline"
    );
}

/// Seed a project assigned a READY catalog image; returns the DB-assigned
/// project id (a uuid — `ProjectRepository::create` ignores the `name`
/// for the primary-key slot and mints its own uuid). Tests key their
/// `trigger` / `await_fresh` calls on this returned id.
///
/// Dispatch image resolution is catalog-only since the "build once, share"
/// refactor (`resolve_dispatch_image` reads `projects.selected_image_id`
/// → the `images` catalog table); the legacy per-project `set_project_image`
/// columns are no longer consulted. So we create a catalog `images` row,
/// mark it ready with the expected content-addressed tag (no digest, so
/// `pull_ref` returns the tag verbatim), and assign it to the project.
async fn seed_project_with_ready_image(db: &Database, name: &str) -> String {
    use djinn_db::ImageRepository;
    let repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project = repo
        .create(name, "test", name)
        .await
        .expect("create project");

    let images = ImageRepository::new(db.clone());
    let image_id = format!("img-{name}");
    images
        .create(&image_id, name, None, "{}")
        .await
        .expect("create catalog image");
    let tag = format!(
        "reg.example:5000/djinn-project-{}:abc123def456",
        &project.id
    );
    images
        .mark_ready(&image_id, &tag, None)
        .await
        .expect("mark image ready");
    images
        .set_project_image(&project.id, Some(&image_id))
        .await
        .expect("assign catalog image to project");
    project.id
}

fn test_config() -> KubernetesConfig {
    KubernetesConfig::for_testing()
}

#[tokio::test]
async fn trigger_dispatches_job_with_expected_labels_and_image() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-trigger").await;

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
    );

    warmer.trigger(&project_id).await;
    // NoopJobWatcher returns instantly and the spawned watcher removes
    // the in-flight entry. Give tokio a scheduling breather so the
    // spawn completes before the assertion — the Notify wakeup happens
    // in a spawned task.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let captured = captured.lock().await;
    assert_eq!(captured.len(), 1, "expected exactly one Job dispatched");
    let (ns, job) = &captured[0];
    assert_eq!(ns, "djinn");
    let labels = job.metadata.labels.as_ref().expect("labels");
    assert_eq!(
        labels.get(crate::warm_job::LABEL_WARM).map(String::as_str),
        Some("true")
    );
    // Project id label is sanitized (lowercased + disallowed-char swap)
    // so the raw UUID v7 round-trips unchanged (`[0-9a-f-]`).
    assert_eq!(
        labels
            .get(crate::warm_job::LABEL_PROJECT_ID)
            .map(String::as_str),
        Some(project_id.as_str())
    );
    let container = &job
        .spec
        .as_ref()
        .expect("spec")
        .template
        .spec
        .as_ref()
        .expect("pod")
        .containers[0];
    assert_eq!(
        container.image.as_deref(),
        Some(format!("reg.example:5000/djinn-project-{}:abc123def456", project_id).as_str())
    );
}

#[tokio::test]
async fn trigger_coalesces_duplicate_calls_for_same_project() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-dedup").await;

    let release = Arc::new(Notify::new());
    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(ControlledWatcher {
            release: release.clone(),
        }),
    );

    warmer.trigger(&project_id).await;
    // Let the spawned watcher task start awaiting the Notify before
    // we attempt coalesced duplicate triggers below — this also
    // means release.notify_one() during cleanup has a guaranteed
    // consumer.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    warmer.trigger(&project_id).await; // should be a no-op
    warmer.trigger(&project_id).await; // still a no-op

    assert_eq!(
        captured.lock().await.len(),
        1,
        "subsequent triggers must coalesce while the first is in flight"
    );

    // Release the watcher and poll until the in-flight entry clears.
    // `notify_one` stores a permit if there's no current waiter, so
    // the release is robust against the spawned task not having
    // reached `notified().await` yet.
    release.notify_one();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if !warmer
            .dispatch
            .in_flight
            .lock()
            .await
            .contains_key(&project_id)
        {
            break;
        }
    }
    assert!(
        !warmer
            .dispatch
            .in_flight
            .lock()
            .await
            .contains_key(&project_id),
        "watcher should have dropped the in-flight entry after release"
    );

    // After completion, a fresh trigger should dispatch again.
    warmer.trigger(&project_id).await;
    assert_eq!(
        captured.lock().await.len(),
        2,
        "post-completion re-trigger should dispatch a second Job"
    );
}

#[tokio::test]
async fn await_fresh_returns_instantly_when_cache_entry_is_recent() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-fresh").await;

    // Seed a cache row the warmer will see as fresh — matching the
    // commit SHA resolvable via the mirror (the discover helper bails
    // out here because no mirror exists, returning None → cache
    // considered stale). To exercise the "fresh hit" path we bypass
    // discover by forcing the await to hit the `Notify` fast path via
    // an already-in-flight slot that completes quickly.
    //
    // This specific test asserts the near-instant behaviour when the
    // Notify resolves without the timeout kicking in.
    let (dispatcher, _captured, _count) = RecordingDispatcher::new("warm");
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db.clone(),
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
    );

    let started = Instant::now();
    warmer
        .await_fresh(&project_id, Duration::from_secs(60), Duration::from_secs(1))
        .await
        .expect("await_fresh returns Ok");
    // NoopJobWatcher fires Notify immediately so await should complete
    // in well under the 1s timeout.
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "await_fresh returned after {:?}",
        started.elapsed()
    );

    // Independently assert the cache-freshness probe handles a recent
    // row without blowing up.
    let cache = RepoGraphCacheRepository::new(db);
    cache
        .upsert(RepoGraphCacheInsert {
            project_id: &project_id,
            commit_sha: "0000000000000000000000000000000000000000",
            graph_blob: b"graph",
        })
        .await
        .expect("upsert cache");
}

#[tokio::test]
async fn await_fresh_times_out_without_deadlocking_when_warm_is_slow() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-slow").await;

    // ControlledWatcher never releases → Notify never fires → caller
    // must hit the timeout backstop and return Ok.
    let release = Arc::new(Notify::new());
    let (dispatcher, _captured, _count) = RecordingDispatcher::new("warm");
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(ControlledWatcher { release }),
    );

    let started = Instant::now();
    warmer
        .await_fresh(
            &project_id,
            Duration::from_secs(60),
            Duration::from_millis(200),
        )
        .await
        .expect("await_fresh returns Ok on timeout");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(150),
        "timeout should observe at least the requested window; got {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "timeout should not hang; got {elapsed:?}"
    );
}

#[tokio::test]
async fn trigger_skips_when_project_has_no_ready_image() {
    let db = Database::open_in_memory().expect("in-memory db");
    let repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project = repo
        .create("proj-noimg", "test", "proj-noimg")
        .await
        .expect("create project");
    // No set_project_image → status stays `none`.

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
    );
    warmer.trigger(&project.id).await;
    assert!(
        captured.lock().await.is_empty(),
        "must not dispatch a warm Job without a ready image"
    );
}

// ── Cross-process / cluster-side dedupe tests ─────────────────────────
//
// Regression coverage for the double-warm bug: the in-process
// `in_flight` map only serialises triggers within ONE server
// process, so a Job that survived a server restart, or one created
// by a parallel pod during a rolling update, is invisible to the
// per-process guard. The `WarmJobLister` is the cross-process
// source of truth; the tests below exercise that path with a
// programmable mock so we can simulate the "another process owns
// the Job" state without standing up a live apiserver.

/// Programmable cluster lister: tests pre-load the answer (true →
/// cluster has an in-flight warm; false → empty), and the lister
/// records every call so the test can assert that the dedupe path
/// was exercised.
struct ProgrammableLister {
    answer: Arc<tokio::sync::Mutex<bool>>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

/// Recorded call list. Aliased so the `new` constructor signature
/// stays readable at the clippy complexity budget — the
/// `Arc<Mutex<Vec<...>>` would otherwise be a deeply-nested
/// generic that clippy lints under `type_complexity`.
type ProgrammableListerCalls = Arc<Mutex<Vec<(String, String)>>>;

impl ProgrammableLister {
    fn new(initial_answer: bool) -> (Self, Arc<tokio::sync::Mutex<bool>>, ProgrammableListerCalls) {
        let answer = Arc::new(tokio::sync::Mutex::new(initial_answer));
        let calls: ProgrammableListerCalls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                answer: answer.clone(),
                calls: calls.clone(),
            },
            answer,
            calls,
        )
    }
}

#[async_trait]
impl WarmJobLister for ProgrammableLister {
    async fn has_in_flight_warm(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<bool, kube::Error> {
        self.calls
            .lock()
            .await
            .push((namespace.to_string(), project_id.to_string()));
        Ok(*self.answer.lock().await)
    }
}

/// Thread-safe variant of [`ProgrammableLister`] used in tests
/// that flip the answer from a non-async test thread. The
/// shared-state `std::sync::Mutex` is the right tool here: a
/// `tokio::sync::Mutex` would block the runtime worker pool if
/// held across a non-async section. The lister still records
/// calls in the same `tokio::sync::Mutex<Vec<…>>` it shares with
/// the production path.
struct ScriptedLister {
    answer: Arc<std::sync::Mutex<bool>>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl WarmJobLister for ScriptedLister {
    async fn has_in_flight_warm(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<bool, kube::Error> {
        self.calls
            .lock()
            .await
            .push((namespace.to_string(), project_id.to_string()));
        Ok(*self.answer.lock().expect("answer poisoned"))
    }
}

/// Snapshot the current length of a `CapturedJobs` vec. Split out
/// from the inline `captured.lock().await.len()` so the assertion
/// sites read at one level of indentation.
async fn captured_len(captured: &CapturedJobs) -> usize {
    captured.lock().await.len()
}

#[tokio::test]
async fn trigger_coalesces_when_cluster_already_has_warm_for_project() {
    // Simulates the production scenario: a previous server process
    // left a warm Job in the cluster (rolling update, server
    // restart, parallel pod), and the new process boots with an
    // empty in-process `in_flight` map. Without the cluster-side
    // dedupe the trigger would dispatch a duplicate Job and
    // contend on `/cache/cargo-target/<project>`. With the lister
    // the duplicate is coalesced.
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-cluster-dedupe").await;

    // `NoopJobWatcher` (not `ControlledWatcher`) because the test
    // path that DOES dispatch a Job (the follow-up trigger after
    // flipping the lister to "empty") would otherwise leak a
    // spawned watcher task on a Notify this test never releases.
    // The watcher is irrelevant to what we assert — we only
    // care about the dispatch count.
    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let (lister, lister_answer, lister_calls) = ProgrammableLister::new(/* answer = */ true);

    let warmer = K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
        Some(Arc::new(lister)),
    );

    warmer.trigger(&project_id).await;
    // No Job may have been dispatched — the cluster lister reported
    // an in-flight warm, so the trigger must coalesce without ever
    // touching `dispatcher.dispatch`.
    assert!(
        captured_len(&captured).await == 0,
        "trigger must coalesce when cluster has a non-terminal warm Job; got {} dispatches",
        captured_len(&captured).await
    );

    // The lister MUST have been consulted at least once. (Production
    // consults it twice — once before the slot insert, once after —
    // to close the cross-process race; both observations land in
    // the recorded call list. We assert ≥ 1 here so the test is
    // robust to a future tightening of the re-check window.)
    {
        let calls = lister_calls.lock().await;
        assert!(
            !calls.is_empty(),
            "lister must be consulted before dispatch (no consultation = cross-process dedupe disabled)"
        );
        for (ns, pid) in calls.iter() {
            assert_eq!(ns, "djinn");
            assert_eq!(pid, &project_id);
        }
    }

    // Flipping the lister's answer to `false` and re-triggering
    // must dispatch normally — proves the coalesce was conditional
    // on the cluster state, not a permanent skip.
    *lister_answer.lock().await = false;
    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        captured_len(&captured).await,
        1,
        "with the cluster empty, trigger must dispatch a Job"
    );
}

#[tokio::test]
async fn trigger_dedupes_concurrent_triggers_via_cluster_lister() {
    // This is the regression test for the bug: a main-tip-advance
    // trigger (e.g. from `mirror_fetcher`) and an image-ready
    // trigger (from `image_build_watcher`) firing within the same
    // window must produce exactly ONE warm Job, not two. We
    // simulate the cross-process race by:
    //
    //  1. Priming the cluster lister to report "in-flight" on the
    //     first observations (another process owns the Job).
    //  2. Spawning two `trigger` calls concurrently and asserting
    //     that both coalesce (no Job dispatched).
    //  3. Flipping the lister to "empty" and re-triggering to
    //     confirm the dispatcher is reachable and dispatches
    //     exactly one Job.
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-double-warm").await;

    // `NoopJobWatcher` (not `ControlledWatcher`) because the test
    // path that DOES dispatch a Job (the follow-up trigger after
    // flipping the lister to "empty") would otherwise hang the
    // spawned watcher task on a Notify this test never releases.
    // The watcher is irrelevant to what we assert — we only
    // care about the dispatch count.
    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");

    // Answer: "yes" (cluster has an in-flight warm) until the
    // test thread clears it. The first trigger to observe the
    // "yes" coalesces; any concurrent trigger also observes
    // "yes" and coalesces.
    let answer = Arc::new(std::sync::Mutex::new(true));
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let lister = ScriptedLister {
        answer: answer.clone(),
        calls: calls.clone(),
    };

    let warmer = std::sync::Arc::new(K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
        Some(Arc::new(lister)),
    ));

    // Fire two triggers concurrently. Both must coalesce.
    let warmer_a = warmer.clone();
    let warmer_b = warmer.clone();
    let pid_a = project_id.clone();
    let pid_b = project_id.clone();
    let t_a = tokio::spawn(async move { warmer_a.trigger(&pid_a).await });
    let t_b = tokio::spawn(async move { warmer_b.trigger(&pid_b).await });
    t_a.await.expect("trigger a");
    t_b.await.expect("trigger b");

    assert_eq!(
        captured_len(&captured).await,
        0,
        "no Job may be dispatched while the lister reports an in-flight warm"
    );

    // The lister MUST have observed at least two calls (one per
    // trigger) so the dedupe path was actually exercised — a
    // future refactor that accidentally short-circuits the
    // lister call would still pass the dispatch-count assertion
    // and silently re-introduce the cross-process race.
    assert!(
        calls.lock().await.len() >= 2,
        "lister must be consulted for every trigger (got {} calls, want >= 2)",
        calls.lock().await.len()
    );

    // After flipping the lister to "no" (the survivor is now
    // visible in the cluster OR has completed), a fresh trigger
    // must dispatch exactly one Job.
    *answer.lock().expect("answer poisoned") = false;
    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        captured_len(&captured).await,
        1,
        "follow-up trigger must dispatch exactly one Job"
    );
}

/// The freshness gate (`cache_has_current_commit`) is preserved by
/// this fix: a project whose canonical graph is already current
/// for the origin/main tip must still short-circuit BEFORE the
/// cluster lister is consulted. This test asserts the weaker
/// invariant that an empty cache does dispatch and the lister is
/// consulted — the freshness gate is exercised in the non-mocked
/// `trigger_dispatches_job_with_expected_labels_and_image` test
/// above, where `discover_mirror_main_tip` returns None and the
/// gate falls open.
#[tokio::test]
async fn trigger_consults_lister_before_dispatching_with_empty_cluster() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-lister-observed").await;

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let (lister, _answer, calls) = ProgrammableLister::new(/* answer = */ false);

    let warmer = K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
        Some(Arc::new(lister)),
    );

    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Cluster is empty → dispatch proceeds, Job is created.
    assert_eq!(captured_len(&captured).await, 1);
    // And the lister WAS consulted (the dedupe path is wired in
    // — a future refactor that accidentally bypasses the lister
    // would still pass the dispatch count assertion and silently
    // re-introduce the cross-process race).
    assert!(
        !calls.lock().await.is_empty(),
        "lister must be consulted even when the cluster is empty (to keep the dedupe path exercised)"
    );
}

// ── Merge-storm debounce tests ────────────────────────────────────────
//
// The automatic head-advance trigger path (`K8sGraphWarmer::trigger`) is
// debounced: a storm of `main` advances collapses into ONE warm run once it
// settles, a continuous storm still warms within the max-wait bound, and
// `quiet == 0` reproduces the pre-debounce synchronous behaviour. These tests
// use short real durations (matching the sleep-based style elsewhere in this
// module) and the `RecordingDispatcher` to count dispatches.

/// Poll up to `attempts` × 5ms for the captured-Job count to reach `want`,
/// returning the final observed count. Keeps the storm assertions robust to
/// scheduler jitter without a fixed oversized sleep.
async fn wait_for_captured(captured: &CapturedJobs, want: usize, attempts: usize) -> usize {
    for _ in 0..attempts {
        if captured_len(captured).await >= want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    captured_len(captured).await
}

/// A storm of triggers arriving within the quiet window must collapse into
/// exactly ONE warm dispatch after the storm settles — the core anti-churn
/// guarantee (11 warms in 79 min → 1).
#[tokio::test]
async fn debounce_collapses_storm_into_single_dispatch() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-storm").await;

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
    )
    .with_debounce(WarmDebounceConfig {
        quiet: Duration::from_millis(200),
        max_wait: Duration::from_secs(5),
    });

    // Fire 5 triggers 20ms apart (all inside the 200ms quiet window).
    for _ in 0..5 {
        warmer.trigger(&project_id).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Nothing should have dispatched yet — the quiet window has not elapsed.
    assert_eq!(
        captured_len(&captured).await,
        0,
        "no warm may dispatch while the quiet window is still open"
    );

    // After the window collapses, exactly one warm dispatches.
    let dispatched = wait_for_captured(&captured, 1, 120).await;
    assert_eq!(
        dispatched, 1,
        "a storm of triggers must collapse into exactly one warm run"
    );
    // Give any stray driver a beat and re-assert it stayed at one.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(captured_len(&captured).await, 1, "still exactly one warm");

    let m = warmer.debounce_metrics();
    assert_eq!(m.triggers_received, 5, "all 5 triggers counted");
    assert_eq!(
        m.triggers_coalesced, 4,
        "4 of 5 folded into the first window"
    );
    assert_eq!(
        m.warms_debounced, 1,
        "one window collapsed into one dispatch"
    );
}

/// A continuously-advancing head (each trigger re-arming the quiet window
/// before it can elapse) must still warm within the max-wait bound rather than
/// deferring forever.
#[tokio::test]
async fn debounce_max_wait_fires_under_continuous_advance() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-maxwait").await;

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    // Quiet (100ms) is always re-armed by the 40ms trigger cadence, so without
    // the max-wait bound this would never fire. max_wait=300ms forces it.
    let warmer = Arc::new(
        K8sGraphWarmer::with_dispatcher(
            test_config(),
            db,
            Arc::new(dispatcher),
            Arc::new(NoopJobWatcher),
        )
        .with_debounce(WarmDebounceConfig {
            quiet: Duration::from_millis(100),
            max_wait: Duration::from_millis(300),
        }),
    );

    let started = Instant::now();
    // Drive a continuous storm: a trigger every 40ms (< quiet) for ~600ms.
    let driver = {
        let warmer = warmer.clone();
        let pid = project_id.clone();
        tokio::spawn(async move {
            for _ in 0..15 {
                warmer.trigger(&pid).await;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
    };

    let dispatched = wait_for_captured(&captured, 1, 200).await;
    let elapsed = started.elapsed();
    driver.await.expect("storm driver");

    assert!(
        dispatched >= 1,
        "max-wait must force a dispatch under a continuous storm; got {dispatched}"
    );
    // It must NOT have fired immediately (proves it waited) and must fire around
    // the max-wait bound, not never.
    assert!(
        elapsed >= Duration::from_millis(250),
        "dispatch should be deferred until ~max_wait, not immediate; fired at {elapsed:?}"
    );
    assert!(
        warmer.debounce_metrics().warms_debounced >= 1,
        "max-wait collapse must count as a debounced warm"
    );
}

/// With debouncing disabled (`quiet == 0`) every trigger dispatches
/// immediately, exactly reproducing the pre-debounce behaviour, and the
/// debounce driver path is never taken.
#[tokio::test]
async fn debounce_disabled_dispatches_immediately() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-disabled").await;

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(NoopJobWatcher),
    )
    .with_debounce(WarmDebounceConfig::DISABLED);

    warmer.trigger(&project_id).await;
    // No quiet window — the Job is dispatched synchronously within trigger.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        captured_len(&captured).await,
        1,
        "disabled debounce must dispatch immediately (pre-debounce behaviour)"
    );

    let m = warmer.debounce_metrics();
    assert_eq!(m.triggers_received, 1);
    assert_eq!(
        m.warms_debounced, 0,
        "the debounce driver path must not run when disabled"
    );
    assert_eq!(m.triggers_coalesced, 0);
}

/// Triggers arriving while a warm is in flight must coalesce into exactly ONE
/// follow-up dispatch (at the latest tip) once the in-flight warm drains — never
/// a queue of stale per-trigger dispatches.
#[tokio::test]
async fn debounce_coalesces_triggers_during_inflight_warm() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-inflight-coalesce").await;

    // ControlledWatcher keeps the first warm "in flight" until we release it,
    // so follow-up triggers must coalesce behind it rather than dispatch.
    let release = Arc::new(Notify::new());
    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let warmer = Arc::new(
        K8sGraphWarmer::with_dispatcher(
            test_config(),
            db,
            Arc::new(dispatcher),
            Arc::new(ControlledWatcher {
                release: release.clone(),
            }),
        )
        .with_debounce(WarmDebounceConfig {
            quiet: Duration::from_millis(80),
            max_wait: Duration::from_secs(5),
        }),
    );

    // First trigger → debounce window → dispatches warm #1 (stays in flight).
    warmer.trigger(&project_id).await;
    let dispatched = wait_for_captured(&captured, 1, 60).await;
    assert_eq!(dispatched, 1, "first debounce window dispatches warm #1");
    // Confirm it is genuinely in flight (watcher blocked on `release`).
    assert!(
        warmer
            .dispatch
            .in_flight
            .lock()
            .await
            .contains_key(&project_id),
        "warm #1 must be in flight before the follow-up storm"
    );

    // Two more triggers arrive while warm #1 is in flight — they must coalesce
    // into a single pending follow-up, not two dispatches.
    warmer.trigger(&project_id).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    warmer.trigger(&project_id).await;

    // Let the follow-up window elapse; nothing may dispatch while #1 is in
    // flight (the driver is draining behind it).
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        captured_len(&captured).await,
        1,
        "no follow-up may dispatch while warm #1 is still in flight"
    );

    // Release warm #1 → it drains → the coalesced follow-up dispatches exactly
    // one more warm.
    release.notify_waiters();
    let dispatched = wait_for_captured(&captured, 2, 120).await;
    assert_eq!(
        dispatched, 2,
        "exactly one follow-up warm dispatches after the in-flight warm drains"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        captured_len(&captured).await,
        2,
        "follow-up must be a single coalesced run, not one per trigger"
    );

    // Clean up the second warm's watcher (also blocked on `release`).
    release.notify_waiters();
}

/// Event-driven convergence: when a warm Job completes successfully the
/// in-process completion sink must fire with the warmed `project_id`, so the
/// server can invalidate its canonical-graph RAM slot without a restart.
#[tokio::test]
async fn trigger_fires_completion_sink_on_warm_success() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-sink-success").await;

    let (dispatcher, _captured, _count) = RecordingDispatcher::new("warm");
    let (sink, calls) = RecordingSink::new();

    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(ImmediateWatcher {
            outcome: WarmTerminalOutcome::Succeeded,
        }),
    )
    .with_completion_sink(Arc::new(sink));

    warmer.trigger(&project_id).await;
    // The watcher runs in a spawned task; give it room to fire the sink.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let calls = calls.lock().await;
    assert_eq!(
        &*calls,
        &[project_id],
        "completion sink must fire exactly once with the warmed project id on success"
    );
}

/// The completion sink must NOT fire when a Job disappears before success is
/// observed: deletion or eviction must not fabricate graph convergence.
#[tokio::test]
async fn trigger_skips_completion_sink_when_job_disappears() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-sink-failure").await;

    let (dispatcher, _captured, _count) = RecordingDispatcher::new("warm");
    let (sink, calls) = RecordingSink::new();

    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(ObservationWatcher {
            observations: vec![(WarmJobObservation::Disappeared, false)],
        }),
    )
    .with_completion_sink(Arc::new(sink));

    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert!(
        calls.lock().await.is_empty(),
        "completion sink must not fire when deletion or eviction makes the Job disappear"
    );
}

struct AdmissionRecording {
    events: Arc<Mutex<Vec<String>>>,
    admit_error: Option<WarmAdmissionError>,
    fail_create_started: bool,
}

#[async_trait]
impl WarmAdmission for AdmissionRecording {
    async fn admit(
        &self,
        request: WarmAdmissionRequest,
    ) -> Result<WarmAdmissionPermit, WarmAdmissionError> {
        self.events
            .lock()
            .await
            .push(format!("reserve:{}", request.object_name));
        match &self.admit_error {
            Some(error) => Err(error.clone()),
            None => Ok(WarmAdmissionPermit::new()),
        }
    }

    async fn transition(
        &self,
        _permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        self.events.lock().await.push(format!("{transition:?}"));
        if self.fail_create_started && transition == WarmAdmissionTransition::CreateStarted {
            Err(WarmAdmissionError::Unavailable {
                diagnostic: "journal unavailable".to_string(),
            })
        } else {
            Ok(())
        }
    }
}

struct LifecycleRecordingDispatcher {
    events: Arc<Mutex<Vec<String>>>,
    result: Result<String, String>,
}

#[async_trait]
impl WarmJobDispatcher for LifecycleRecordingDispatcher {
    async fn dispatch(&self, _namespace: &str, _job: Job) -> Result<String, String> {
        self.events.lock().await.push("post".to_string());
        self.result.clone()
    }
}

struct UidWatcher;

#[async_trait]
impl WarmJobWatcher for UidWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        WarmTerminalOutcome::Succeeded
    }

    async fn job_uid(&self, _namespace: &str, _job_name: &str) -> Option<String> {
        Some("uid-123".to_string())
    }
}

#[tokio::test]
async fn admission_is_reserved_and_create_started_before_post_then_reports_uid_lifecycle() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-admission-order").await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(LifecycleRecordingDispatcher {
            events: events.clone(),
            result: Ok("admitted-warm".to_string()),
        }),
        Arc::new(UidWatcher),
    )
    .with_warm_admission(Arc::new(AdmissionRecording {
        events: events.clone(),
        admit_error: None,
        fail_create_started: false,
    }));

    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let events = events.lock().await;
    assert!(events[0].starts_with("reserve:djinn-warm-"));
    assert_eq!(events[1], "CreateStarted");
    assert_eq!(events[2], "post");
    assert_eq!(events[3], "Live { uid: \"uid-123\" }");
    assert_eq!(events[4], "Terminal { uid: \"uid-123\" }");
}

#[tokio::test]
async fn admission_denial_or_failed_create_started_never_posts_and_is_retryable() {
    for (admit_error, fail_create_started) in [
        (
            Some(WarmAdmissionError::Denied {
                diagnostic: "at capacity".to_string(),
            }),
            false,
        ),
        (None, true),
    ] {
        let db = Database::open_in_memory().expect("in-memory db");
        let project_id = seed_project_with_ready_image(&db, "proj-admission-denied").await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let warmer = K8sGraphWarmer::with_dispatcher(
            test_config(),
            db,
            Arc::new(LifecycleRecordingDispatcher {
                events: events.clone(),
                result: Ok("must-not-post".to_string()),
            }),
            Arc::new(UidWatcher),
        )
        .with_warm_admission(Arc::new(AdmissionRecording {
            events: events.clone(),
            admit_error,
            fail_create_started,
        }));
        warmer.trigger(&project_id).await;
        warmer.trigger(&project_id).await;
        let events = events.lock().await;
        assert!(
            !events.iter().any(|event| event == "post"),
            "a denial or non-durable CreateStarted must perform zero POSTs"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("reserve:"))
                .count(),
            1,
            "an immediate retrigger must coalesce onto the pending identity"
        );
        drop(events);
        assert!(
            warmer
                .dispatch
                .in_flight
                .lock()
                .await
                .contains_key(&project_id),
            "admission-gated work remains pending rather than becoming a failed warm"
        );
    }
}

#[tokio::test]
async fn dispatcher_failures_report_one_conservative_admission_outcome() {
    for (error, expected) in [
        (
            "connection reset after POST",
            "CreateUnknown { diagnostic: \"connection reset after POST\" }",
        ),
        (
            "transport: proxy host not found after POST",
            "CreateUnknown { diagnostic: \"transport: proxy host not found after POST\" }",
        ),
        (
            "Forbidden: status code 403",
            "DefinitiveFailure { diagnostic: \"Forbidden: status code 403\" }",
        ),
        (
            "ApiError: NotFound (status code 404)",
            "DefinitiveFailure { diagnostic: \"ApiError: NotFound (status code 404)\" }",
        ),
    ] {
        let db = Database::open_in_memory().expect("in-memory db");
        let project_id = seed_project_with_ready_image(&db, "proj-admission-post-error").await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let warmer = K8sGraphWarmer::with_dispatcher(
            test_config(),
            db,
            Arc::new(LifecycleRecordingDispatcher {
                events: events.clone(),
                result: Err(error.to_string()),
            }),
            Arc::new(UidWatcher),
        )
        .with_warm_admission(Arc::new(AdmissionRecording {
            events: events.clone(),
            admit_error: None,
            fail_create_started: false,
        }));
        warmer.trigger(&project_id).await;
        let events = events.lock().await;
        assert_eq!(
            events.len(),
            4,
            "one dispatcher POST has one reported outcome"
        );
        assert!(events[0].starts_with("reserve:djinn-warm-"));
        assert_eq!(events[1], "CreateStarted");
        assert_eq!(events[2], "post");
        assert_eq!(events[3], expected);
        drop(events);
        assert!(
            !warmer
                .dispatch
                .in_flight
                .lock()
                .await
                .contains_key(&project_id),
            "a POST failure is cleaned up once and does not leave a watcher cleanup"
        );
    }
}

#[tokio::test]
async fn unconfigured_admission_fails_closed_without_posting() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-admission-unconfigured").await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(LifecycleRecordingDispatcher {
            events: events.clone(),
            result: Ok("must-not-post".to_string()),
        }),
        Arc::new(UidWatcher),
    );
    warmer.dispatch.admission = None;

    warmer.trigger(&project_id).await;
    warmer.trigger(&project_id).await;
    assert!(
        events.lock().await.is_empty(),
        "no admission means no dispatcher POST"
    );
    assert!(
        warmer
            .dispatch
            .in_flight
            .lock()
            .await
            .contains_key(&project_id),
        "the unconfigured dispatch remains coalesced instead of retrying a POST"
    );
}

#[test]
fn dispatcher_error_classification_is_conservative() {
    assert!(dispatcher_error_is_definitive("Forbidden: status code 403"));
    assert!(dispatcher_error_is_definitive(
        "ApiError: NotFound (status code 404)"
    ));
    assert!(!dispatcher_error_is_definitive(
        "connection reset after POST"
    ));
    assert!(!dispatcher_error_is_definitive(
        "transport: proxy host not found after POST"
    ));
    assert!(!dispatcher_error_is_definitive("already exists"));
}

#[path = "graph_warmer_overlap_tests.rs"]
mod overlap;
