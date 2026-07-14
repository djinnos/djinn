// Focused object-overlap lifecycle tests. Kept separate to preserve the
// graph-warmer test module's file-size guard while sharing its deterministic
// dispatcher/lister/watcher seam.
#![allow(clippy::disallowed_methods)]

use super::*;
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// ── Object-level overlap: dedupe is an optimisation, not a writer mutex ──
//
// These tests establish the *real* correctness boundary for Job overlap:
// independent warmer processes (separate `in_flight` maps) can both observe
// "no in-flight Job" and both dispatch, because cluster observation is racy
// (list/create race) or fails open (API error). Correctness under overlap comes
// from the worker's per-project PVC advisory lock
// (`/cache/cargo-target/.warm-locks/<project-id>.lock`, merged in task `t6g0`),
// which serialises prune/stamp/compile across overlapping Pods and is released
// on normal completion or process death. The tests below do NOT claim the
// in-process map or the label lister is a single-writer guarantee.

/// A lister whose `has_in_flight_warm` always returns `Err`, simulating a
/// transient apiserver failure. The warmer path treats this as fail-open
/// (returns `false`), proving that errors cannot wedge the cluster but CAN
/// let overlapping Jobs dispatch.
struct FailingLister {
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl WarmJobLister for FailingLister {
    async fn has_in_flight_warm(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<bool, kube::Error> {
        self.calls
            .lock()
            .await
            .push((namespace.to_string(), project_id.to_string()));
        // Simulate an apiserver error — the warmer fails open (returns false).
        Err(kube::Error::Service(
            std::io::Error::other("connection refused (test simulation)").into(),
        ))
    }
}

/// A lister that always returns `Ok(false)` — the cluster reports "no
/// in-flight warm" regardless of state, simulating a list/create race where
/// both processes observe "empty" before either's Job appears in the listing.
struct AlwaysEmptyLister {
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl WarmJobLister for AlwaysEmptyLister {
    async fn has_in_flight_warm(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<bool, kube::Error> {
        self.calls
            .lock()
            .await
            .push((namespace.to_string(), project_id.to_string()));
        Ok(false)
    }
}

/// Two independent warmer instances (separate `in_flight` maps, simulating two
/// server processes) can both dispatch a warm Job for the same project when the
/// cluster lister fails open (API error). The dedupe path was exercised (both
/// instances consulted the lister), but neither saw an in-flight Job, so both
/// dispatched — proving that the lister is an optimisation, not a writer mutex.
#[tokio::test]
async fn independent_warmers_both_dispatch_when_lister_fails_open() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-overlap-failopen").await;

    // Each warmer gets its own dispatcher and its own `in_flight` map.
    let (dispatcher_a, captured_a, _count_a) = RecordingDispatcher::new("warm-a");
    let (dispatcher_b, captured_b, _count_b) = RecordingDispatcher::new("warm-b");

    // A shared failing lister — both warmers consult it, both get Err, both
    // fail open and proceed to dispatch.
    let lister_calls = Arc::new(Mutex::new(Vec::new()));
    let warmer_a = K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db.clone(),
        Arc::new(dispatcher_a),
        Arc::new(NoopJobWatcher),
        Some(Arc::new(FailingLister {
            calls: lister_calls.clone(),
        })),
    );
    let warmer_b = K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db,
        Arc::new(dispatcher_b),
        Arc::new(NoopJobWatcher),
        Some(Arc::new(FailingLister {
            calls: lister_calls.clone(),
        })),
    );

    warmer_a.trigger(&project_id).await;
    warmer_b.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    // BOTH warmers dispatched — the lister failed open, so neither coalesced.
    assert_eq!(
        captured_len(&captured_a).await,
        1,
        "warmer A must dispatch despite the lister error (fail-open)"
    );
    assert_eq!(
        captured_len(&captured_b).await,
        1,
        "warmer B must also dispatch — two independent instances both observed 'no Job'"
    );
    // The lister WAS consulted by both warmers — the dedupe path ran, it just
    // could not prevent overlap because the observation failed.
    assert!(
        lister_calls.lock().await.len() >= 2,
        "both warmers must consult the lister; got {} calls",
        lister_calls.lock().await.len()
    );
}

/// Two independent warmer instances can both dispatch for one project when a
/// list/create race makes the cluster report "empty" to both — neither sees the
/// other's Job because neither has been created yet at observation time. This
/// is the canonical object-level overlap path that the PVC advisory lock (not
/// scheduler dedupe) makes safe.
#[tokio::test]
async fn independent_warmers_both_dispatch_in_list_create_race() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-overlap-race").await;

    let (dispatcher_a, captured_a, _count_a) = RecordingDispatcher::new("warm-a");
    let (dispatcher_b, captured_b, _count_b) = RecordingDispatcher::new("warm-b");

    // Both warmers share a lister that always reports "empty" — simulating the
    // list/create race where neither Job has been committed to the listing yet.
    let lister_calls = Arc::new(Mutex::new(Vec::new()));
    let shared_lister: Arc<dyn WarmJobLister> = Arc::new(AlwaysEmptyLister {
        calls: lister_calls.clone(),
    });

    let warmer_a = K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db.clone(),
        Arc::new(dispatcher_a),
        Arc::new(NoopJobWatcher),
        Some(shared_lister.clone()),
    );
    let warmer_b = K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db,
        Arc::new(dispatcher_b),
        Arc::new(NoopJobWatcher),
        Some(shared_lister),
    );

    // Fire both triggers — both observe "empty cluster" and both dispatch.
    warmer_a.trigger(&project_id).await;
    warmer_b.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert_eq!(
        captured_len(&captured_a).await,
        1,
        "warmer A dispatches in the list/create race"
    );
    assert_eq!(
        captured_len(&captured_b).await,
        1,
        "warmer B also dispatches — overlap is possible because listing is racy"
    );
    assert!(
        lister_calls.lock().await.len() >= 2,
        "both warmers consulted the lister"
    );
}

/// Deletion/eviction replacement scenario:
///
/// 1. A warm Job is dispatched and then deleted/evicted (Pod still terminating).
///    Its watcher observes `Disappeared` → `WarmTerminalOutcome::Failed`, so the
///    completion sink does NOT fire (no fabricated graph convergence).
/// 2. The in-flight slot is released by the failed watcher, so a replacement
///    warm can dispatch — even though the predecessor Pod may still be
///    terminating (object-level overlap). The lister reports "empty" because
///    the old Job is already gone from the listing.
/// 3. The replacement warm subsequently succeeds and fires the completion sink.
///
/// This proves: (a) deletion/eviction does not fabricate success convergence;
/// (b) a replacement may dispatch while predecessor termination is unresolved;
/// (c) the replacement can complete successfully — the PVC advisory lock (task
/// `t6g0`) guarantees the overlapping Pods never corrupt the shared base.
#[tokio::test]
async fn deletion_replacement_old_fails_new_succeeds() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-replacement").await;

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let (sink, sink_calls) = RecordingSink::new();

    // A scripted lister: initially "empty" (the old Job hasn't appeared or has
    // already been deleted from the listing). This permits both the initial
    // dispatch and the replacement dispatch.
    let lister_calls = Arc::new(Mutex::new(Vec::new()));
    let lister: Arc<dyn WarmJobLister> = Arc::new(AlwaysEmptyLister {
        calls: lister_calls.clone(),
    });

    // Watcher that simulates deletion/eviction: the first observed Job
    // disappears (WarmTerminalOutcome::Failed), then the replacement succeeds.
    // We use a per-call counter to distinguish the two watcher invocations.
    let watch_call_count = Arc::new(AtomicUsize::new(0));

    struct ReplacementWatcher {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WarmJobWatcher for ReplacementWatcher {
        async fn wait_terminal(&self, _ns: &str, _job: &str) -> WarmTerminalOutcome {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First Job: deleted/evicted before success was observed.
                WarmTerminalOutcome::Failed
            } else {
                // Replacement Job: completes successfully.
                WarmTerminalOutcome::Succeeded
            }
        }
    }

    let warmer = K8sGraphWarmer::with_dispatcher_and_lister(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(ReplacementWatcher {
            call_count: watch_call_count,
        }),
        Some(lister),
    )
    .with_completion_sink(Arc::new(sink));

    // Phase 1: dispatch the initial warm Job.
    warmer.trigger(&project_id).await;
    // Let the spawned watcher run — it returns Failed (disappeared).
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The initial attempt must NOT have fired the completion sink.
    assert!(
        sink_calls.lock().await.is_empty(),
        "deleted/evicted warm must not fabricate graph convergence (sink must be empty)"
    );
    // The initial Job WAS dispatched.
    assert_eq!(
        captured_len(&captured).await,
        1,
        "the initial (doomed) warm Job must have been dispatched"
    );

    // The in-flight slot must have been released by the failed watcher.
    for _ in 0..50 {
        if !warmer
            .dispatch
            .in_flight
            .lock()
            .await
            .contains_key(&project_id)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !warmer
            .dispatch
            .in_flight
            .lock()
            .await
            .contains_key(&project_id),
        "the failed watcher must release the in-flight slot so a replacement can dispatch"
    );

    // Phase 2: dispatch the replacement warm — the predecessor Pod may still be
    // terminating, but the lister reports "empty" (old Job already deleted).
    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The replacement dispatched (object-level overlap with the predecessor's
    // terminating Pod is possible — the PVC lock serialises the actual work).
    assert_eq!(
        captured_len(&captured).await,
        2,
        "the replacement warm must dispatch after the predecessor failed"
    );

    // Phase 3: the replacement succeeds and fires the completion sink exactly
    // once — only the successful warm converges the graph.
    let sink_snapshot = {
        let guard = sink_calls.lock().await;
        guard.clone()
    };
    assert_eq!(
        sink_snapshot,
        vec![project_id.clone()],
        "only the successful replacement warm must fire the completion sink"
    );

    // The lister was consulted for both the initial and replacement dispatches.
    assert!(
        lister_calls.lock().await.len() >= 2,
        "the lister must be consulted for both the initial and replacement dispatches"
    );
}

/// A failed warm Job (active-deadline expiry or explicit failure) does NOT fire
/// the completion sink, and a subsequent replacement dispatch can succeed. This
/// complements the deletion scenario by covering the explicit-failure terminal
/// (not just disappearance): the outcome is still `Failed`, no convergence.
#[tokio::test]
async fn failed_warm_does_not_converge_but_replacement_can_succeed() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-failed-replace").await;

    let (dispatcher, captured, _count) = RecordingDispatcher::new("warm");
    let (sink, sink_calls) = RecordingSink::new();

    let call_count = Arc::new(AtomicUsize::new(0));

    struct FailThenSucceedWatcher {
        count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WarmJobWatcher for FailThenSucceedWatcher {
        async fn wait_terminal(&self, _ns: &str, _job: &str) -> WarmTerminalOutcome {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                WarmTerminalOutcome::Failed
            } else {
                WarmTerminalOutcome::Succeeded
            }
        }
    }

    let warmer = K8sGraphWarmer::with_dispatcher(
        test_config(),
        db,
        Arc::new(dispatcher),
        Arc::new(FailThenSucceedWatcher { count: call_count }),
    )
    .with_completion_sink(Arc::new(sink));

    // First warm: fails (active-deadline or explicit failure).
    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(captured_len(&captured).await, 1, "first warm dispatched");
    assert!(
        sink_calls.lock().await.is_empty(),
        "a failed warm must not fire the completion sink"
    );

    // Replacement: succeeds.
    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(captured_len(&captured).await, 2, "replacement dispatched");
    let snapshot = sink_calls.lock().await.clone();
    assert_eq!(
        snapshot,
        vec![project_id],
        "the successful replacement fires the completion sink exactly once"
    );
}
