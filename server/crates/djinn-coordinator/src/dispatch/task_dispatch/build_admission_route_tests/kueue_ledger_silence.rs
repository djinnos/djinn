//! AC6 (task `o53p`): deleting the pre-create pods-quota reservation ledger did
//! not delete the pre-Kueue dispatch path along with it.
//!
//! With `kueue.armed = false` — the shipped default, and the configuration
//! every deployment runs today — a task-run, a graph-warm Job and a SCIP-index
//! Job must each still be dispatched. That is the whole claim: the removal was
//! a subtraction of an authority, not a subtraction of dispatch.
//!
//! # Why these tests assert a dispatch count and a row census together
//!
//! "Nothing writes to the ledger any more" is trivially satisfiable by a build
//! in which nothing dispatches at all, and that failure mode is strictly worse
//! than the one the deletion set out to fix. So every helper here returns the
//! dispatch count alongside the census, and every test asserts the count FIRST.
//! A run that produced no Job never gets as far as the census assertions.
//!
//! Conversely a census alone would be blind to the direction of travel, so the
//! relations are counted SEPARATELY and never summed: a write to one hiding
//! behind an absent write to another is precisely the shape the old two-stacked-
//! authorities bug had.
//!
//! `admission_journal` is deliberately NOT in [`LEDGER_RELATIONS`]. The relation
//! is being dropped with the authority that owned it; counting it would make
//! these tests fail to compile against the migrated schema for a reason that has
//! nothing to do with dispatch. `build_leases` (the build-slot lease, retained)
//! and `admission_handoff` (the physical row the invocation-lease authority
//! lives in, retained) are what is left, and both are still reachable from these
//! paths.

use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{BuildLeaseRepository, Database, ImageRepository, ProjectRepository};
use djinn_k8s::scip_schedule::MirrorHead;
use djinn_k8s::{
    K8sGraphWarmer, KubernetesConfig, ScipIndexScheduler, ScipJobInventory, ScipJobObservation,
    WarmJobDispatcher, WarmJobManifest, WarmJobWatcher, WarmTerminalOutcome,
};

use super::*;
use crate::build_lease::BuildLeaseService;
use crate::graph_warm_lease::BuildLeaseGraphWarmAdapter;
use djinn_runtime::GraphWarmerService;

/// The ledger relations that survive `o53p`. Counted individually, never summed.
const LEDGER_RELATIONS: [&str; 2] = ["build_leases", "admission_handoff"];

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

fn census_of(census: &[(&'static str, i64)], relation: &str) -> i64 {
    census
        .iter()
        .find(|(name, _)| *name == relation)
        .unwrap_or_else(|| panic!("{relation} counted"))
        .1
}

// ---------------------------------------------------------------------------
// Task-run dispatch
// ---------------------------------------------------------------------------

/// Run one production `dispatch_ready_tasks` with `kueue.armed = false` and
/// return `(census before, census after, tasks dispatched, task the slot pool
/// actually created)`.
async fn dispatch_one_task_run() -> (
    Vec<(&'static str, i64)>,
    Vec<(&'static str, i64)>,
    usize,
    Option<String>,
) {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
    let fixture = seed_wnd1_ready_worker_tasks(&db, WND1_READY_TASK_COUNT).await;
    configure_wnd1_user_max_sessions(&db, &fixture.created_by_user_id, &fixture.model_id, 1).await;

    let (runtime, mut started_rx, _completed_rx) = RouteRuntime::new();
    let mut actor = build_route_actor(&db, &events_tx, &runtime, 1);

    let task_id = fixture.task_ids[0].clone();
    close_all_except(&db, &fixture, &task_id).await;

    let before = ledger_census(&db).await;
    actor.dispatch_ready_tasks(Some(&fixture.project_id)).await;
    let dispatched = actor.dispatched;
    // The create side effect, not just the coordinator's own counter: `started`
    // is sent from inside the slot-pool runner.
    let created = if dispatched > 0 {
        Some(
            started_rx
                .recv()
                .await
                .expect("the pool runner fires for a dispatched task"),
        )
    } else {
        None
    };
    let after = ledger_census(&db).await;
    if let Some(created) = created.as_deref() {
        runtime.release(created).await;
    }
    (before, after, dispatched as usize, created)
}

/// Disarmed, a task-run still dispatches all the way into the slot pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disarmed_task_run_still_dispatches() {
    let (before, after, dispatched, created) = dispatch_one_task_run().await;

    assert_eq!(
        dispatched, 1,
        "deleting the reservation ledger must not stop a task-run from being \
         dispatched on the pre-Kueue path"
    );
    assert!(
        created.is_some(),
        "the coordinator counted a dispatch the slot pool never created"
    );
    for ((relation, start), (_, end)) in before.iter().zip(after.iter()) {
        assert_eq!(
            start, end,
            "a disarmed task-run dispatch must not reserve {relation} before the \
             create (before={start}, after={end})"
        );
    }
}

// ---------------------------------------------------------------------------
// Graph-warm dispatch
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

/// Drive one production warm dispatch with the retained v1 build lease wired,
/// and return `(census before, census after, POST count)`.
async fn dispatch_one_warm() -> (Vec<(&'static str, i64)>, Vec<(&'static str, i64)>, usize) {
    let db = Database::open_in_memory().expect("test database");
    let project_id = seed_project_with_ready_image(&db, "kueue-warm-ledger").await;

    let lease_service = StdArc::new(BuildLeaseService::new(
        StdArc::new(BuildLeaseRepository::new(db.clone())),
        4,
    ));
    lease_service.recover().await;

    let mut config = KubernetesConfig::for_testing();
    config.kueue_armed = false;
    // The LEASED path polls `wait_for_bind_and_open_leased_candidate` for
    // `warm_job_timeout_seconds` awaiting a Kubernetes candidate this harness
    // deliberately does not provide. The 7200s default would hang the run. Every
    // ledger write this test counts happens BEFORE the POST, and so before that
    // wait is entered — shortening it changes nothing about what is being
    // proven.
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

/// Disarmed, a warm Job is still created — and the authority that admits it is
/// the retained v1 build lease, which must still be reached.
///
/// The `build_leases` growth assertion is the non-vacuity control: without it
/// this test would also pass for a build in which the lease seam was never
/// wired, and then the POST count would be measuring nothing but the fake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disarmed_warm_job_still_dispatches_through_the_retained_lease() {
    let (before, after, posts) = dispatch_one_warm().await;

    assert_eq!(
        posts, 1,
        "deleting the reservation ledger must not stop the warm Job from being \
         created"
    );
    assert!(
        census_of(&after, "build_leases") > census_of(&before, "build_leases"),
        "the warm dispatch must still pass through the retained v1 build lease \
         (before={}, after={})",
        census_of(&before, "build_leases"),
        census_of(&after, "build_leases")
    );
    assert_eq!(
        census_of(&after, "admission_handoff"),
        census_of(&before, "admission_handoff"),
        "a warm dispatch never mutates the invocation-lease authority row"
    );
}

// ---------------------------------------------------------------------------
// SCIP-index dispatch
// ---------------------------------------------------------------------------

/// Cluster inventory holding no SCIP and no warm Job for the project, so
/// `decide` reaches its dispatch arm.
struct EmptyScipInventory;

#[async_trait]
impl ScipJobInventory for EmptyScipInventory {
    async fn observe(
        &self,
        _namespace: &str,
        _project_id: &str,
    ) -> Result<ScipJobObservation, String> {
        Ok(ScipJobObservation::default())
    }
}

/// Drive one production SCIP-index dispatch and return
/// `(census before, census after, POST count)`.
async fn dispatch_one_scip() -> (Vec<(&'static str, i64)>, Vec<(&'static str, i64)>, usize) {
    let db = Database::open_in_memory().expect("test database");
    let project_id = seed_project_with_ready_image(&db, "kueue-scip-ledger").await;

    let mut config = KubernetesConfig::for_testing();
    config.kueue_armed = false;

    let posts = StdArc::new(AtomicUsize::new(0));
    let scheduler = ScipIndexScheduler::new(
        config,
        StdArc::new(EmptyScipInventory),
        StdArc::new(CountingWarmDispatcher {
            posts: StdArc::clone(&posts),
        }),
    );

    // A head that has stood still far longer than the quiescence floor, so the
    // pure `decide` reaches `Dispatch` rather than a skip arm.
    let head = MirrorHead {
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        age: std::time::Duration::from_secs(24 * 60 * 60),
    };

    let before = ledger_census(&db).await;
    let decision = scheduler
        .tick_project(
            &project_id,
            Some(&head),
            "reg.example:5000/djinn-project-scip:abc123",
            None,
        )
        .await;
    assert_eq!(
        decision.dispatch_revision(),
        Some(head.revision.as_str()),
        "the SCIP scheduler must reach its dispatch arm, not a skip arm \
         (reason: {})",
        decision.reason()
    );
    let after = ledger_census(&db).await;
    (before, after, posts.load(Ordering::SeqCst))
}

/// Disarmed, a SCIP-index Job is still created — and, as it always was, it is
/// created leaselessly. Its CPU is folded into protected capacity, so it must
/// touch neither surviving ledger relation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disarmed_scip_job_still_dispatches_without_taking_a_lease() {
    let (before, after, posts) = dispatch_one_scip().await;

    assert_eq!(
        posts, 1,
        "deleting the reservation ledger must not stop the SCIP-index Job from \
         being created"
    );
    for ((relation, start), (_, end)) in before.iter().zip(after.iter()) {
        assert_eq!(
            start, end,
            "the SCIP Job is leaseless by construction; it must write no \
             {relation} rows (before={start}, after={end})"
        );
    }
}
