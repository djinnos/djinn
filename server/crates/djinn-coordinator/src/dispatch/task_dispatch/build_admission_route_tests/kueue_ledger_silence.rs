//! AC4 (task `37yq`): armed, a dispatch writes NOTHING to the build-admission
//! ledger — and disarmed, it writes exactly what it wrote before.
//!
//! # Why these tests count rows instead of inspecting code
//!
//! "The call site is gone" is not a property you can assert about this system.
//! The warm path carries TWO stacked capacity authorities — the v1 build lease
//! (`build_leases`) and the admission journal (`admission_journal`) — and they
//! are written from different call sites in `graph_warmer.rs`. A grep-shaped or
//! mock-shaped assertion that one of them is silent passes while the other is
//! still appending rows, still counting occupancy against a cap nothing
//! decrements, and still able to refuse a dispatch Kueue has already admitted.
//!
//! So each test takes a per-relation row census against a real Postgres test
//! database, runs the production dispatch, and takes it again. Every relation is
//! counted SEPARATELY: a single summed total would let a write to one relation
//! hide behind an absent write to another.
//!
//! `admission_handoff` is counted too even though the dispatch path does not
//! write it today. That is the point — it is the relation an epoch handoff
//! writes, and a "zero writes" claim that never looked at it is a claim about
//! two thirds of the ledger.

use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{
    AdmissionJournalRepository, BuildLeaseRepository, Database, ImageRepository, ProjectRepository,
};
use djinn_k8s::{
    K8sGraphWarmer, KubernetesConfig, WarmJobDispatcher, WarmJobManifest, WarmJobWatcher,
    WarmTerminalOutcome,
};

use super::*;
use crate::build_admission::{BuildAdmissionController, BuildAdmissionMode};
use crate::build_lease::BuildLeaseService;
use crate::graph_warm_lease::BuildLeaseGraphWarmAdapter;
use djinn_runtime::GraphWarmerService;

/// The three relations the build-admission capacity authority persists to.
/// Counted individually, never summed.
const LEDGER_RELATIONS: [&str; 3] = ["build_leases", "admission_journal", "admission_handoff"];

/// Per-relation row census, in the order of [`LEDGER_RELATIONS`].
async fn ledger_census(db: &Database) -> Vec<(&'static str, i64)> {
    let mut census = Vec::new();
    for relation in LEDGER_RELATIONS {
        census.push((
            relation,
            djinn_db::test_support::count_rows_for_test(db, relation).await,
        ));
    }
    census
}

// ---------------------------------------------------------------------------
// Task-run dispatch
// ---------------------------------------------------------------------------

/// Run one production `dispatch_ready_tasks` with the ledger fully wired, and
/// return `(census before, census after, tasks dispatched)`.
async fn dispatch_one_task_run(
    kueue_armed: bool,
) -> (Vec<(&'static str, i64)>, Vec<(&'static str, i64)>, usize) {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
    let fixture = seed_wnd1_ready_worker_tasks(&db, WND1_READY_TASK_COUNT).await;
    configure_wnd1_user_max_sessions(&db, &fixture.created_by_user_id, &fixture.model_id, 1).await;

    let journal = route_journal(&db);
    let harness = route_controller(&db, &journal, BuildAdmissionMode::Enforce, 1).await;
    let controller = StdArc::clone(&harness.controller);
    let (runtime, mut started_rx, _completed_rx) = AdmissionRouteRuntime::new(journal.clone());

    let mut actor = build_route_actor(&db, &events_tx, &runtime, 1);
    actor.build_admission = Some(controller.clone());
    // The ONLY difference between the two runs. Everything above is the
    // production wiring in both.
    actor.kueue_armed = kueue_armed;

    let task_id = fixture.task_ids[0].clone();
    close_all_except(&db, &fixture, &task_id).await;

    let before = ledger_census(&db).await;
    actor.dispatch_ready_tasks(Some(&fixture.project_id)).await;
    let dispatched = actor.dispatched;
    if dispatched > 0 {
        let observed = started_rx
            .recv()
            .await
            .expect("the pool runner fires for a dispatched task");
        assert_eq!(observed, task_id);
    }
    let after = ledger_census(&db).await;
    if dispatched > 0 {
        runtime.release(&task_id).await;
    }
    (before, after, dispatched as usize)
}

/// Armed, a task-run dispatch still dispatches — and writes ZERO rows to all
/// three ledger relations.
///
/// The `dispatched == 1` assertion is load-bearing: a gate that simply refused
/// to dispatch would also write no rows, and would be a far worse bug than the
/// one this replaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_task_run_dispatch_writes_no_ledger_rows() {
    let (before, after, dispatched) = dispatch_one_task_run(true).await;

    assert_eq!(
        dispatched, 1,
        "arming Kueue must not stop the dispatch — Kueue admits it, the ledger \
         simply stops reserving for it"
    );
    for ((relation, start), (_, end)) in before.iter().zip(after.iter()) {
        assert_eq!(
            start, end,
            "armed, a task-run dispatch must write no {relation} rows \
             (before={start}, after={end})"
        );
    }
}

/// Disarmed, the SAME dispatch still writes to the journal. Without this the
/// test above passes for a build where the ledger was never reachable at all —
/// a broken harness and a working cutover look identical from one side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disarmed_task_run_dispatch_still_writes_the_pre_existing_row_set() {
    let (before, after, dispatched) = dispatch_one_task_run(false).await;

    assert_eq!(dispatched, 1, "the disarmed dispatch must still dispatch");
    let journal_before = before
        .iter()
        .find(|(relation, _)| *relation == "admission_journal")
        .expect("journal counted")
        .1;
    let journal_after = after
        .iter()
        .find(|(relation, _)| *relation == "admission_journal")
        .expect("journal counted")
        .1;
    assert!(
        journal_after > journal_before,
        "disarmed, the pre-existing write set is unchanged: the dispatch must \
         still append to admission_journal (before={journal_before}, \
         after={journal_after})"
    );
}

// ---------------------------------------------------------------------------
// Graph-warm dispatch — the two stacked authorities
// ---------------------------------------------------------------------------

struct CountingWarmDispatcher {
    posts: StdArc<AtomicUsize>,
}

#[async_trait]
impl WarmJobDispatcher for CountingWarmDispatcher {
    async fn dispatch(&self, _namespace: &str, job: WarmJobManifest) -> Result<String, String> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        Ok(job.metadata.name.clone().unwrap_or_else(|| "warm".into()))
    }
}

struct ImmediateSuccessWatcher;

#[async_trait]
impl WarmJobWatcher for ImmediateSuccessWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        WarmTerminalOutcome::Succeeded
    }
}

async fn seed_project_with_ready_image(db: &Database, name: &str) -> String {
    let projects = ProjectRepository::new(db.clone(), EventBus::noop());
    let project = projects.create(name, "test", name).await.unwrap();
    let images = ImageRepository::new(db.clone());
    let image_id = format!("img-{name}");
    images.create(&image_id, name, None, "{}").await.unwrap();
    images
        .mark_ready(
            &image_id,
            &format!("reg.example:5000/djinn-project-{}:abc123", project.id),
            None,
        )
        .await
        .unwrap();
    images
        .set_project_image(&project.id, Some(&image_id))
        .await
        .unwrap();
    project.id
}

/// Drive one production warm dispatch with BOTH capacity authorities wired —
/// the v1 build lease and the admission journal — and return
/// `(census before, census after, POST count)`.
async fn dispatch_one_warm(
    kueue_armed: bool,
) -> (Vec<(&'static str, i64)>, Vec<(&'static str, i64)>, usize) {
    let db = Database::open_in_memory().expect("test database");
    let project_id = seed_project_with_ready_image(&db, "kueue-warm-ledger").await;

    let journal = StdArc::new(AdmissionJournalRepository::new(db.clone()));
    let controller = StdArc::new(BuildAdmissionController::new(
        StdArc::clone(&journal),
        BuildAdmissionMode::Observe,
        4,
        "kueue-ledger-epoch",
    ));

    let lease_service = StdArc::new(BuildLeaseService::new(
        StdArc::new(BuildLeaseRepository::new(db.clone())),
        4,
    ));
    lease_service.recover().await;

    let mut config = KubernetesConfig::for_testing();
    config.kueue_armed = kueue_armed;
    // The LEASED path polls `wait_for_bind_and_open_leased_candidate` for
    // `warm_job_timeout_seconds` awaiting a Kubernetes candidate this harness
    // deliberately does not provide. The 7200s default would hang the disarmed
    // run. Every ledger write this test counts happens BEFORE the POST, and so
    // before that wait is entered — shortening it changes nothing about what is
    // being proven. (Same reasoning as
    // `graph_warmer_ledger_tests::a_lease_granted_warm_job_is_still_recorded_in_the_lifecycle_ledger`.)
    config.warm_job_timeout_seconds = 1;

    let posts = StdArc::new(AtomicUsize::new(0));
    let warmer = K8sGraphWarmer::with_dispatcher(
        config,
        db.clone(),
        StdArc::new(CountingWarmDispatcher {
            posts: StdArc::clone(&posts),
        }),
        StdArc::new(ImmediateSuccessWatcher),
    )
    .with_warm_admission(controller)
    .with_graph_warm_lease(StdArc::new(BuildLeaseGraphWarmAdapter::new(StdArc::clone(
        &lease_service,
    ))));

    let before = ledger_census(&db).await;
    warmer.trigger(&project_id).await;
    // The warm dispatch's terminal transition runs on a detached task; give it a
    // bounded window so a late write cannot be mistaken for no write at all.
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        if ledger_census(&db).await != before {
            break;
        }
    }
    let after = ledger_census(&db).await;
    (before, after, posts.load(Ordering::SeqCst))
}

/// Armed, a warm dispatch creates its Job and writes ZERO rows to all three
/// relations.
///
/// The lease AND the journal are both wired here. Silencing only one would fail
/// this test on the other's relation — which is exactly the failure a
/// call-site-shaped assertion cannot produce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_warm_dispatch_writes_no_ledger_rows_through_either_authority() {
    let (before, after, posts) = dispatch_one_warm(true).await;

    assert_eq!(
        posts, 1,
        "arming Kueue must not stop the warm Job from being created"
    );
    for ((relation, start), (_, end)) in before.iter().zip(after.iter()) {
        assert_eq!(
            start, end,
            "armed, a warm dispatch must write no {relation} rows \
             (before={start}, after={end})"
        );
    }
}

/// Disarmed, the same warm dispatch writes to BOTH authorities. This is the
/// control that makes the test above mean something, and it is also the direct
/// evidence for "two stacked authorities": one dispatch, two relations growing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disarmed_warm_dispatch_writes_through_both_stacked_authorities() {
    let (before, after, posts) = dispatch_one_warm(false).await;

    assert_eq!(
        posts, 1,
        "the disarmed warm dispatch must still POST its Job"
    );
    for relation in ["build_leases", "admission_journal"] {
        let start = before
            .iter()
            .find(|(name, _)| *name == relation)
            .expect("relation counted")
            .1;
        let end = after
            .iter()
            .find(|(name, _)| *name == relation)
            .expect("relation counted")
            .1;
        assert!(
            end > start,
            "disarmed, the warm dispatch must still write {relation} \
             (before={start}, after={end}) — otherwise the armed test above is \
             asserting against an authority that was never reachable"
        );
    }
}
