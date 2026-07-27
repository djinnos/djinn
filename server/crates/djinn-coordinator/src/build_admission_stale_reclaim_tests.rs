//! Regression coverage for stale durable occupancy whose Kubernetes objects
//! are gone (production symptom: 318 occupying `admission_journal` rows against
//! a namespace holding zero Jobs, with `djinn_build_slots_in_use` reading 318
//! while `kubectl get jobs -A` returned nothing).
//!
//! Every stale row in these tests is produced by the production dispatch path —
//! the concrete `K8sGraphWarmer` driving the production `BuildAdmissionController`
//! and the production `AdmissionJournalRepository` against a real project row in
//! a fresh database — and then aged into its stale form by the production
//! `recover_all_predecessors_and_seed` restart path. Nothing here hand-writes a
//! journal row and then asks reconciliation to clean it up: the seeding is the
//! behaviour under test just as much as the reclamation is.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{
    AdmissionDomain, AdmissionJournalKey, AdmissionJournalRepository, AdmissionState,
    AdmissionWorkloadKind, CreateStartedInput, Database, ImageRepository, ProjectRepository,
    ReserveAdmissionInput, TerminalAdmissionInput, UidFencedAdmissionInput,
};
use djinn_k8s::{
    K8sGraphWarmer, KubernetesConfig, ObjectPresence, UidGetResult, WarmAdmission,
    WarmAdmissionError, WarmAdmissionRequest, WarmJobDispatcher, WarmJobManifest, WarmJobWatcher,
    WarmTerminalOutcome, WorkloadInventory, WorkloadObjectKind, WorkloadRecord, warm_work_id,
};
use djinn_runtime::GraphWarmerService;
use tokio::sync::RwLock;

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode, BuildAdmissionReadiness,
    BuildAdmissionRequest, BuildWorkloadKind, CapacitySource, DenialCause,
};
use crate::build_admission_capacity_support::{CapacityHarness, attach_capacity};
use crate::build_admission_inventory::BuildAdmissionReconciler;

const PREDECESSOR_EPOCH: &str = "epoch-before-the-restart";
const REPLACEMENT_EPOCH: &str = "epoch-after-the-restart";

/// An inventory whose namespace holds exactly the Jobs it was given.
///
/// `presence` answers authoritatively from that same set, which is what a live
/// API server does: an object either exists under a name or it does not.
struct NamespaceInventory {
    records: RwLock<Vec<WorkloadRecord>>,
}

impl NamespaceInventory {
    fn empty() -> Self {
        Self {
            records: RwLock::new(Vec::new()),
        }
    }

    fn holding(records: Vec<WorkloadRecord>) -> Self {
        Self {
            records: RwLock::new(records),
        }
    }
}

#[async_trait]
impl WorkloadInventory for NamespaceInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Ok(self.records.read().await.clone())
    }

    async fn get_uid(&self, _kind: WorkloadObjectKind, name: &str, uid: &str) -> UidGetResult {
        match self
            .records
            .read()
            .await
            .iter()
            .find(|record| record.name == name)
        {
            Some(record) if record.uid.as_deref() == Some(uid) => UidGetResult::Present,
            Some(_) => UidGetResult::Uncertain,
            None => UidGetResult::NotFound,
        }
    }

    async fn presence(&self, _kind: WorkloadObjectKind, name: &str) -> ObjectPresence {
        match self
            .records
            .read()
            .await
            .iter()
            .find(|record| record.name == name)
        {
            Some(record) => ObjectPresence::Present {
                uid: record.uid.clone(),
            },
            None => ObjectPresence::Absent,
        }
    }
}

/// A dispatcher whose POST response is lost. This is the production shape that
/// leaves a generation in an ambiguous create state: Kubernetes may or may not
/// have created the object, so the warmer records `CreateUnknown` rather than a
/// definitive failure.
struct LostResponseDispatcher;

#[async_trait]
impl WarmJobDispatcher for LostResponseDispatcher {
    async fn dispatch(&self, _namespace: &str, _job: WarmJobManifest) -> Result<String, String> {
        Err("connection reset by peer while awaiting the create response".into())
    }
}

/// A dispatcher that POSTs successfully, paired with a watcher that reports a
/// UID and then never terminalizes — the process is lost mid-lifecycle.
struct SucceedingDispatcher;

#[async_trait]
impl WarmJobDispatcher for SucceedingDispatcher {
    async fn dispatch(&self, _namespace: &str, _job: WarmJobManifest) -> Result<String, String> {
        Ok("warm-job".into())
    }
}

struct NeverTerminalWatcher {
    uid: String,
}

#[async_trait]
impl WarmJobWatcher for NeverTerminalWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        std::future::pending::<()>().await;
        unreachable!("the observing process is lost before the Job terminates")
    }

    async fn job_uid(&self, _namespace: &str, _job_name: &str) -> Option<String> {
        Some(self.uid.clone())
    }
}

async fn seed_project(db: &Database, name: &str) -> String {
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

async fn await_state(
    journal: &AdmissionJournalRepository,
    work_id: &str,
    expected: AdmissionState,
) {
    for _ in 0..600 {
        let history = journal
            .list_history(AdmissionDomain::WarmBuild, work_id)
            .await
            .unwrap();
        if history.last().is_some_and(|row| row.state == expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "warm work {work_id} never reached {expected:?}: {:?}",
        journal
            .list_history(AdmissionDomain::WarmBuild, work_id)
            .await
            .unwrap()
    );
}

/// Drive real warm dispatches on a doomed process and return the durable shape
/// production accumulated: `ambiguous` generations left in an unresolved create
/// and `orphaned` generations left Live with a UID nobody will ever retire.
///
/// The returned work ids are the journal's own, not a test fixture's.
async fn accumulate_production_stale_population(
    db: &Database,
    journal: &Arc<AdmissionJournalRepository>,
    ambiguous: usize,
    orphaned: usize,
) -> Vec<String> {
    let doomed = Arc::new(BuildAdmissionController::new(
        Arc::clone(journal),
        // Production ran Observe while this population accumulated.
        BuildAdmissionMode::Observe,
        3,
        PREDECESSOR_EPOCH,
    ));
    let mut work_ids = Vec::new();
    for index in 0..ambiguous {
        let project_id = seed_project(db, &format!("ambiguous-{index}")).await;
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db.clone(),
            Arc::new(LostResponseDispatcher),
            Arc::new(NeverTerminalWatcher {
                uid: format!("unused-{index}"),
            }),
        )
        .with_warm_admission(doomed.clone());
        warmer.trigger(&project_id).await;
        let work_id = warm_work_id(&project_id, "unknown");
        await_state(journal, &work_id, AdmissionState::CreateUnknown).await;
        work_ids.push(work_id);
    }
    for index in 0..orphaned {
        let project_id = seed_project(db, &format!("orphaned-{index}")).await;
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db.clone(),
            Arc::new(SucceedingDispatcher),
            Arc::new(NeverTerminalWatcher {
                uid: format!("orphan-uid-{index}"),
            }),
        )
        .with_warm_admission(doomed.clone());
        warmer.trigger(&project_id).await;
        let work_id = warm_work_id(&project_id, "unknown");
        await_state(journal, &work_id, AdmissionState::Live).await;
        work_ids.push(work_id);
    }
    work_ids
}

/// The replacement process, composed the way production composes it: an
/// Enforce controller reaching capacity through the ONE lease authority over
/// the same database. Without the authority the controller is not capacity
/// gated at all, and "the cap still binds after reconciliation" would be a
/// claim nothing could falsify.
async fn replacement(
    db: &Database,
    journal: &Arc<AdmissionJournalRepository>,
    cap: i64,
) -> CapacityHarness {
    let controller = BuildAdmissionController::new(
        Arc::clone(journal),
        BuildAdmissionMode::Enforce,
        cap,
        REPLACEMENT_EPOCH,
    );
    attach_capacity(db, controller, cap).await
}

fn warm_request(id: &str) -> WarmAdmissionRequest {
    WarmAdmissionRequest {
        domain: "ignored".into(),
        work_id: id.into(),
        generation: 0,
        object_name: format!("job-{id}"),
    }
}

/// The production build-admission entry point the warm trait wraps, so the
/// bounded decision (and its occupancy arithmetic) is directly observable.
fn build_request(id: &str) -> BuildAdmissionRequest {
    BuildAdmissionRequest {
        domain: AdmissionDomain::WarmBuild,
        work_id: id.into(),
        generation: 0,
        object_name: format!("job-{id}"),
        kind: BuildWorkloadKind::GraphWarmJob,
        // Warm capacity is the graph-warm lease; this exercise drives the
        // journal ledger and its reclamation, which is what #2597 owns.
        capacity: CapacitySource::HeldByLease,
    }
}

/// The blocker, end to end: a production-shaped stale population denies every
/// Enforce admission at cap 3, and startup reconciliation against an authoritative
/// namespace listing releases it so the board runs again.
#[tokio::test]
async fn production_shaped_stale_population_is_reclaimed_and_enforce_admits_at_cap() {
    let db = Database::open_in_memory().unwrap();
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
    let stale_work = accumulate_production_stale_population(&db, &journal, 4, 2).await;
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        6,
        "the doomed process left six occupying generations behind"
    );

    // The process is replaced. This is the real startup recovery path.
    let h = replacement(&db, &journal, 3).await;
    let controller = Arc::clone(&h.controller);
    let report = controller
        .recover_all_predecessors_and_seed()
        .await
        .unwrap();
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        6,
        "recovery retires Reserved rows and converts CreateInFlight into occupying \
         CreateUnknown; it releases none of this population"
    );
    assert!(
        matches!(
            report.readiness,
            BuildAdmissionReadiness::CreateUnknownHealth
                | BuildAdmissionReadiness::SeededOccupancyAboveCap
        ),
        "the stale population must trip a startup gate, not pass silently: {:?}",
        report.readiness
    );
    controller.mark_inventory_ready();
    controller.mark_topology_ready();
    assert!(
        !controller.is_ready(),
        "Enforce cannot arm while the stale population occupies the cap"
    );
    let wedged = WarmAdmission::admit(controller.as_ref(), warm_request("blocked-by-stale"))
        .await
        .expect_err("this is the production wedge: every admission denied while stale");
    assert!(
        matches!(wedged, WarmAdmissionError::Denied { .. }),
        "unexpected wedge diagnostic: {wedged}"
    );

    // Startup reconciliation against a namespace that holds no Jobs at all —
    // the exact production observation.
    let reconciler = BuildAdmissionReconciler::with_settle_window(
        Arc::clone(&controller),
        Arc::new(NamespaceInventory::empty()),
        Duration::ZERO,
    );
    let inventory = reconciler.reconcile().await;
    assert!(
        inventory.blockers.is_empty(),
        "unexpected blockers: {:?}",
        inventory.blockers
    );
    assert_eq!(
        inventory.stale, 6,
        "every occupying row's object was proven absent"
    );
    assert_eq!(inventory.reclaimed, 4, "four ambiguous creates are retired");
    assert_eq!(inventory.released, 2, "two orphaned Live rows are retired");
    assert_eq!(inventory.fenced, 0);
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        0,
        "reconciliation must release the whole stale population"
    );
    for work_id in &stale_work {
        let history = journal
            .list_history(AdmissionDomain::WarmBuild, work_id)
            .await
            .unwrap();
        assert!(
            history
                .iter()
                .all(|row| row.state == AdmissionState::Terminal),
            "{work_id} retains a non-terminal generation: {history:?}"
        );
    }

    // The board runs again: three concurrent builds admit, the fourth queues.
    controller.mark_topology_ready();
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::Healthy,
        "every startup gate must clear once the stale occupancy is gone"
    );
    // Warm capacity is taken at the graph-warm LEASE the warmer holds before it
    // reaches admission; the admission call is the ledger append that made this
    // population reclaimable in the first place. Asserting the denial at
    // admission would now assert nothing, because `HeldByLease` never consults
    // a cap. So the three grants are asserted where they are actually decided.
    let mut held = Vec::new();
    for index in 0..3 {
        held.push(
            h.hold_warm_lease(&format!("after-{index}"))
                .await
                .unwrap_or_else(|| panic!("slot {index} must be free after reconciliation")),
        );
        let decision = controller
            .admit(build_request(&format!("after-{index}")))
            .await
            .unwrap();
        assert!(
            matches!(decision, BuildAdmissionDecision::Permitted { .. }),
            "admission {index} must be granted at cap 3 after reconciliation: {decision:?}"
        );
    }
    assert_eq!(
        h.occupancy().await,
        3,
        "the reclaimed slots are re-occupied, exactly filling the cap"
    );
    assert!(
        h.hold_warm_lease("after-3").await.is_none(),
        "the cap must still bind after reconciliation; reclamation is not a bypass"
    );
    // Same pool, other population: a build-capable task-run is denied by the
    // three warm Jobs. This is the unification -- before it, dispatch had its
    // own three and the node ran six.
    let fourth = controller
        .admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            "after-3-task".into(),
            0,
            "task-run-after-3".into(),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            fourth,
            BuildAdmissionDecision::Denied {
                occupancy: Some(3),
                cap: 3,
                cause: DenialCause::AtCapacity
            }
        ),
        "the cap must still bind after reconciliation; reclamation is not a bypass: {fourth:?}"
    );
    drop(held);
}

/// Reclamation must not become a way to release work that is still running.
/// Only rows whose object the API server denies the existence of are retired.
#[tokio::test]
async fn reclamation_never_releases_work_that_is_still_fenced() {
    let db = Database::open_in_memory().unwrap();
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
    accumulate_production_stale_population(&db, &journal, 2, 0).await;

    // One of the two ambiguous creates actually landed: the Job exists in the
    // namespace under the name the journal recorded. Its capacity is real.
    let rows = journal.list_active_rows().await.unwrap();
    assert_eq!(rows.len(), 2);
    let surviving = rows[0].clone();
    let surviving_job = WorkloadRecord {
        kind: WorkloadObjectKind::Job,
        name: surviving.object_name.clone(),
        uid: Some("the-create-did-land".into()),
        labels: Default::default(),
        terminal: false,
        images: vec!["djinn:test".into()],
        commands: vec!["djinn-warm".into()],
    };

    let controller = Arc::clone(&replacement(&db, &journal, 3).await.controller);
    controller
        .recover_all_predecessors_and_seed()
        .await
        .unwrap();
    let report = BuildAdmissionReconciler::with_settle_window(
        Arc::clone(&controller),
        Arc::new(NamespaceInventory::holding(vec![surviving_job])),
        Duration::ZERO,
    )
    .reconcile()
    .await;

    assert_eq!(
        report.reclaimed, 1,
        "only the generation whose object is absent may be reclaimed"
    );
    let remaining = journal.list_active_rows().await.unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "the generation whose Job still exists must keep occupying: {remaining:?}"
    );
    assert_eq!(remaining[0].key, surviving.key);
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "live work is never released by reconciliation"
    );
}

/// A row created by THIS process may be mid-create right now, and an API server
/// that cannot answer is not evidence of anything. Neither may be reclaimed.
#[tokio::test]
async fn current_epoch_and_unsettled_rows_are_never_reclaimed() {
    let db = Database::open_in_memory().unwrap();
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));

    // A live process holds an in-flight create of its own. Its epoch matches
    // the reconciling controller's, so no Kubernetes evidence can retire it.
    let controller = Arc::clone(&replacement(&db, &journal, 3).await.controller);
    controller.mark_ready();
    let project_id = seed_project(&db, "in-flight-now").await;
    let warmer = K8sGraphWarmer::with_dispatcher(
        KubernetesConfig::for_testing(),
        db.clone(),
        Arc::new(LostResponseDispatcher),
        Arc::new(NeverTerminalWatcher {
            uid: "unused".into(),
        }),
    )
    .with_warm_admission(controller.clone());
    warmer.trigger(&project_id).await;
    let work_id = warm_work_id(&project_id, "unknown");
    await_state(&journal, &work_id, AdmissionState::CreateUnknown).await;

    let report = BuildAdmissionReconciler::with_settle_window(
        Arc::clone(&controller),
        Arc::new(NamespaceInventory::empty()),
        Duration::ZERO,
    )
    .reconcile()
    .await;
    assert_eq!(
        report.reclaimed, 0,
        "a row this process created may still be mid-create"
    );
    assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 1);

    // Same rows, seen by a replacement process, but inside the settle window:
    // the API server could still be admitting a create the dead process POSTed.
    let successor = Arc::new(BuildAdmissionController::new(
        Arc::clone(&journal),
        BuildAdmissionMode::Enforce,
        3,
        "a-third-epoch",
    ));
    successor.recover_all_predecessors_and_seed().await.unwrap();
    let report = BuildAdmissionReconciler::with_settle_window(
        Arc::clone(&successor),
        Arc::new(NamespaceInventory::empty()),
        Duration::from_secs(3600),
    )
    .reconcile()
    .await;
    assert_eq!(
        report.reclaimed, 0,
        "an unsettled row is not yet safe to judge by a listing"
    );
    assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 1);
}

/// One task's durable generation history, written through the journal
/// primitives a pre-`ymx9` server called.
///
/// Before `ymx9` the caller supplied the generation (a task's `reopen_count`)
/// and the journal trusted it, so a dispatch could open generation N+1 while
/// generation N was still `live`. Post-`ymx9` resolution cannot produce that
/// shape any more, but production is full of rows that predate it: at the time
/// of writing, 58 `live` `task_observation` rows, every one of them superseded
/// by a later generation. `reserve` with an explicit generation IS the
/// pre-`ymx9` call, so this is the production writer, not a fixture.
async fn pre_ymx9_generation_history(
    journal: &AdmissionJournalRepository,
    work_id: &str,
    generations: &[(i64, bool)],
) {
    for (generation, terminal) in generations {
        let key = AdmissionJournalKey {
            domain: AdmissionDomain::TaskObservation,
            work_id: work_id.into(),
            generation: *generation,
        };
        let object_name = format!("task-run-{work_id}-{generation}");
        let object_uid = format!("uid-{work_id}-{generation}");
        let reserved = journal
            .reserve(&ReserveAdmissionInput {
                key: key.clone(),
                workload_kind: AdmissionWorkloadKind::Task,
                creator_server_epoch: PREDECESSOR_EPOCH.into(),
                object_name: object_name.clone(),
            })
            .await
            .unwrap();
        // The ledger append never denies, so a fresh key always yields a
        // non-idempotent reservation. There is no `AtCapacity` outcome to match
        // on any more: capacity is decided by the lease, before this call.
        assert!(!reserved.idempotent);
        journal
            .mark_create_started(&CreateStartedInput {
                key: key.clone(),
                creator_server_epoch: PREDECESSOR_EPOCH.into(),
                object_name,
            })
            .await
            .unwrap();
        journal
            .mark_live(&UidFencedAdmissionInput {
                key: key.clone(),
                object_uid: object_uid.clone(),
            })
            .await
            .unwrap();
        if *terminal {
            journal
                .mark_terminal(&TerminalAdmissionInput {
                    key,
                    object_uid: Some(object_uid),
                })
                .await
                .unwrap();
        }
    }
}

/// The exact production shape behind
/// `invalid transition: stale admission generation 1 for 019f6f04-…`.
///
/// A `live` generation that a later generation superseded is the population
/// most in need of reclaiming, and requiring it to be the latest generation
/// vetoes precisely its own reclamation. Absence still has to be proven — the
/// namespace listing and the direct GET both answer that the object is gone —
/// but a stale generation is not a reason to occupy the cap forever.
#[tokio::test]
async fn superseded_live_generations_are_reclaimed_once_their_object_is_proven_absent() {
    let db = Database::open_in_memory().unwrap();
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
    // The three work ids blocking production reconciliation, generation for
    // generation: a terminal predecessor, an orphaned `live` middle, and a
    // terminal successor that makes the middle row stale.
    for work_id in [
        "019f6f04-b477-7851-8be9-ce2e41453d46",
        "019f6fed-d0c9-7441-bf25-502e6d9e6e2e",
    ] {
        pre_ymx9_generation_history(&journal, work_id, &[(0, true), (1, false), (2, true)]).await;
    }
    // The remaining production row: generation 0 orphaned, 1 and 2 terminal.
    pre_ymx9_generation_history(
        &journal,
        "019f6fec-f41c-7da0-9e14-34cea42b7dd5",
        &[(0, false), (1, true), (2, true)],
    )
    .await;
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        3,
        "three superseded live generations occupy the cap"
    );

    let controller = Arc::clone(&replacement(&db, &journal, 3).await.controller);
    controller
        .recover_all_predecessors_and_seed()
        .await
        .unwrap();
    controller.mark_topology_ready();

    let report = BuildAdmissionReconciler::with_settle_window(
        Arc::clone(&controller),
        Arc::new(NamespaceInventory::empty()),
        Duration::ZERO,
    )
    .reconcile()
    .await;

    assert!(
        report.blockers.is_empty(),
        "a stale generation must not veto its own reclamation: {:?}",
        report.blockers
    );
    assert_eq!(report.reclaim_failure_count, 0);
    assert_eq!(report.stale, 3);
    assert_eq!(report.released, 3);
    assert_eq!(report.fenced, 0);
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        0,
        "the whole superseded population must be retired"
    );
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::Healthy,
        "Enforce must arm on the namespace's current state, not its history"
    );
}

/// One unreclaimable row must cost one row, not the whole pass.
///
/// The unreclaimable row here is a superseded `live` generation whose Job still
/// exists and has completed: the object is present, so the ordinary
/// `mark_terminal` callback applies and its latest-generation fence rejects the
/// write. That rejection is a fact about one row. While it failed the pass, a
/// namespace holding one such object denied every other row's reclamation and
/// left Enforce fail-closed on history rather than current state.
#[tokio::test]
async fn one_unreclaimable_row_costs_one_row_and_the_sweep_continues() {
    let db = Database::open_in_memory().unwrap();
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
    pre_ymx9_generation_history(&journal, "completed-object", &[(0, false), (1, true)]).await;
    for index in 0..3 {
        pre_ymx9_generation_history(
            &journal,
            &format!("absent-object-{index}"),
            &[(0, false), (1, true)],
        )
        .await;
    }
    assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 4);

    // The superseded row's Job is still in the namespace, carries the admission
    // identity the dispatch path stamps on it, and has completed. The object is
    // present, so its ordinary lifecycle callback applies — and that callback
    // is the one whose latest-generation fence rejects a superseded row.
    let completed = WorkloadRecord {
        kind: WorkloadObjectKind::Job,
        name: "task-run-completed-object-0".into(),
        uid: Some("uid-completed-object-0".into()),
        labels: [
            (
                djinn_k8s::LABEL_ADMISSION_DOMAIN.to_string(),
                "task_observation".to_string(),
            ),
            (
                djinn_k8s::LABEL_ADMISSION_WORK_ID.to_string(),
                "completed-object".to_string(),
            ),
            (
                djinn_k8s::LABEL_ADMISSION_GENERATION.to_string(),
                "0".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        terminal: true,
        images: vec!["djinn:test".into()],
        commands: vec!["djinn-agent-worker".into()],
    };

    let controller = Arc::clone(&replacement(&db, &journal, 3).await.controller);
    controller
        .recover_all_predecessors_and_seed()
        .await
        .unwrap();
    let report = BuildAdmissionReconciler::with_settle_window(
        Arc::clone(&controller),
        Arc::new(NamespaceInventory::holding(vec![completed])),
        Duration::ZERO,
    )
    .reconcile()
    .await;

    assert!(
        report.blockers.is_empty(),
        "a per-row reclaim failure is not a pass-level blocker: {:?}",
        report.blockers
    );
    assert_eq!(
        report.reclaim_failure_count, 1,
        "exactly one row could not be retired"
    );
    assert_eq!(report.reclaim_failures.len(), 1);
    assert!(
        report.reclaim_failures[0].contains("completed-object")
            && report.reclaim_failures[0].contains("stale admission generation"),
        "the failure must name the row and its cause: {:?}",
        report.reclaim_failures
    );
    assert_eq!(
        report.released, 3,
        "the three rows whose objects are absent must still be retired"
    );
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "only the unreclaimable row keeps occupying"
    );
}
