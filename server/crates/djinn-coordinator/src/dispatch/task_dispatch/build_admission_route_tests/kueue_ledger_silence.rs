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
use djinn_core::models::LaneMaxSessions;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::{
    BuildLeaseRepository, Database, ImageRepository, ProjectRepository, WarmGraphAttempt,
    WarmGraphAttemptStatus, WarmGraphOutcome,
};
use djinn_k8s::scip_schedule::MirrorHead;
use djinn_k8s::{
    K8sGraphWarmer, KubernetesConfig, ScipIndexScheduler, ScipJobInventory, ScipJobObservation,
    WarmJobDispatcher, WarmJobManifest, WarmJobWatcher, WarmOutcomeSource, WarmTerminalOutcome,
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

/// A pending outer session admission ends before the coordinator can hand work
/// to the reply loop's model-turn preparation/launch boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_pending_precedes_every_model_turn_boundary() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
    let fixture = seed_wnd1_ready_worker_tasks(&db, WND1_READY_TASK_COUNT).await;
    configure_wnd1_user_max_sessions(&db, &fixture.created_by_user_id, &fixture.model_id, 1).await;
    let target_task_id = fixture.task_ids[0].clone();
    close_all_except(&db, &fixture, &target_task_id).await;
    djinn_db::SessionRepository::new(db.clone(), EventBus::noop())
        .create(CreateSessionParams {
            project_id: &fixture.project_id,
            task_id: Some(&fixture.task_ids[1]),
            model: &fixture.model_id,
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("seed active outer session");
    let pool =
        djinn_db::test_support::seed_model_turn_admission_fixture(&db, "enforce", "supported", 1)
            .await;

    let boundary_events = StdArc::new(StdMutex::new(Vec::new()));
    let observed = StdArc::clone(&boundary_events);
    djinn_slot::reply_loop::turn::set_reply_loop_boundary_observer(Some(StdArc::new(
        move |event| {
            observed.lock().expect("boundary events mutex").push(event);
        },
    )));
    let (runtime, mut started_rx, _completed_rx) = RouteRuntime::new();
    let mut actor = build_route_actor(&db, &events_tx, &runtime, 1);
    actor.dispatch_ready_tasks(Some(&fixture.project_id)).await;
    djinn_slot::reply_loop::turn::set_reply_loop_boundary_observer(None);

    assert_eq!(
        actor.dispatched, 0,
        "pending outer admission must not dispatch a slot"
    );
    assert!(
        started_rx.try_recv().is_err(),
        "pending must not schedule a replacement dispatch"
    );
    assert!(
        boundary_events
            .lock()
            .expect("boundary events mutex")
            .is_empty(),
        "pending must precede reply-loop handoff, preparation, and every provider launch"
    );
    assert_eq!(
        djinn_db::test_support::model_turn_decision_count_fixture(&db, pool).await,
        0
    );
    assert_eq!(
        djinn_db::test_support::model_turn_accounting_fixture(&db, pool).await,
        (0, 1, 0)
    );
    assert!(actor.inflight_dispatches.is_empty());
    assert!(actor.provisional_admissions.is_empty());
}

/// A rejected outer lane admission ends at the same coordinator boundary as a
/// pending model admission. An explicit lane ceiling remains a resident
/// rejection, rather than becoming a model-turn decision.
///
/// The model cap deliberately has room for a second session. This makes the
/// existing worker session reject on its role-mapped `implement` lane alone,
/// exercising the other conjunct of the production outer-admission caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_lane_rejection_precedes_every_model_turn_boundary() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
    let fixture = seed_wnd1_ready_worker_tasks(&db, WND1_READY_TASK_COUNT).await;
    configure_wnd1_user_max_sessions(&db, &fixture.created_by_user_id, &fixture.model_id, 2).await;
    djinn_db::UserSettingsRepository::new(db.clone())
        .upsert_lane_max_sessions(
            &fixture.created_by_user_id,
            &LaneMaxSessions {
                plan: 2,
                implement: 1,
                review: 2,
            },
        )
        .await
        .expect("configure full worker lane cap");
    let target_task_id = fixture.task_ids[0].clone();
    close_all_except(&db, &fixture, &target_task_id).await;
    djinn_db::SessionRepository::new(db.clone(), EventBus::noop())
        .create(CreateSessionParams {
            project_id: &fixture.project_id,
            task_id: Some(&fixture.task_ids[1]),
            model: &fixture.model_id,
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("seed active session that fills the worker lane");
    let pool =
        djinn_db::test_support::seed_model_turn_admission_fixture(&db, "enforce", "supported", 1)
            .await;

    let boundary_events = StdArc::new(StdMutex::new(Vec::new()));
    let observed = StdArc::clone(&boundary_events);
    djinn_slot::reply_loop::turn::set_reply_loop_boundary_observer(Some(StdArc::new(
        move |event| {
            observed.lock().expect("boundary events mutex").push(event);
        },
    )));
    let (runtime, mut started_rx, _completed_rx) = RouteRuntime::new();
    let mut actor = build_route_actor(&db, &events_tx, &runtime, 1);
    actor.dispatch_ready_tasks(Some(&fixture.project_id)).await;
    djinn_slot::reply_loop::turn::set_reply_loop_boundary_observer(None);

    assert_eq!(
        actor.dispatched, 0,
        "rejected outer lane admission must not dispatch a slot"
    );
    assert!(
        started_rx.try_recv().is_err(),
        "rejection must not schedule a replacement dispatch"
    );
    assert!(
        boundary_events
            .lock()
            .expect("boundary events mutex")
            .is_empty(),
        "rejection must precede reply-loop handoff, preparation, and every provider launch"
    );
    assert_eq!(
        djinn_db::test_support::model_turn_decision_count_fixture(&db, pool).await,
        0,
        "outer rejection must not persist a model-turn decision"
    );
    assert_eq!(
        djinn_db::test_support::model_turn_accounting_fixture(&db, pool).await,
        (0, 1, 0),
        "outer rejection must leave no pending, acquired, dispatching, or active lease"
    );
    assert!(
        actor.inflight_dispatches.is_empty(),
        "outer rejection must leave no coordinator dispatch reservation"
    );
    assert!(
        actor.provisional_admissions.is_empty(),
        "outer rejection must leave no provisional admission"
    );
}

/// The durable lifecycle is separate from the empty Kubernetes Job inventory.
/// This fixture represents the terminal warm failure that permits SCIP recovery.
struct RecoverableWarmOutcome;

#[async_trait]
impl WarmOutcomeSource for RecoverableWarmOutcome {
    async fn warm_outcome_for_head(
        &self,
        project_id: &str,
        exact_head_revision: &str,
    ) -> Result<WarmGraphOutcome, String> {
        Ok(WarmGraphOutcome::TriedAndDidNotPublish(WarmGraphAttempt {
            attempt_id: "failed-warm-attempt".to_owned(),
            project_id: project_id.to_owned(),
            revision: exact_head_revision.to_owned(),
            status: WarmGraphAttemptStatus::Failed,
            started_at: "2026-01-01T00:00:00Z".to_owned(),
            deadline_at: "2026-01-01T01:00:00Z".to_owned(),
            finished_at: Some("2026-01-01T00:30:00Z".to_owned()),
            detail: Some("fixture warm failure".to_owned()),
        }))
    }
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
    )
    .with_warm_outcome_source(StdArc::new(RecoverableWarmOutcome));

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
            &[],
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
