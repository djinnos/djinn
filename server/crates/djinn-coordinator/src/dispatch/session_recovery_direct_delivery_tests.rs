//! Repository-backed direct-delivery liveness for the two recovery-release
//! seams (`super::*` = `session_recovery`).
//!
//! Both `admit_zombie_session_release` and
//! `admit_execution_state_orphan_release` are the collaborators the actor loops
//! themselves invoke. Each seam gets its **own** matrices below rather than
//! having one seam's safety inferred from the other's coverage — the two release
//! sites are independently reachable in production, so a regression in either
//! must fail on its own.
//!
//! # The hang this coverage originally had to work around (fixed in `i5fn`)
//!
//! `DirectDeliveryEngine::integrate` used to retry `LedgerResult::Stale` in an
//! **unbounded** loop, sleeping 1ms between attempts, on the assumption that
//! staleness only ever means "the selected parent's transaction has not
//! finalized yet" — a transient condition.
//!
//! `TaskRepository::task_integrated` also returns `Stale` for a *permanent*
//! condition: `task.status != "approved"` (it closes `WHERE status='approved'`).
//! The two were conflated, so driving the engine for a task in any other status
//! never terminated — reachable from exactly these seams, because the zombie
//! and orphan loops select `in_progress`, `in_task_review`, and
//! `in_lead_intervention`, none of which is `approved`.
//!
//! `i5fn` split that into `TaskIntegrationStaleness`, so the ledger reports
//! which decline can converge, and bounded the remaining transient wait.
//! [`both_recovery_seams_terminate_on_a_permanently_stale_generation`] is the
//! regression: it drives both seams with the `in_progress` fixture that used to
//! hang them.
//!
//! The other engine-driving tests below still use an `approved` fixture,
//! because that is the only status the contract integrates *from* and they are
//! about what a successful integration does; the admission-only tests keep
//! `in_progress` so a refusal that regressed into a release stays visible.

use super::*;
use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{Database, EpicRepository, ProposalBuildAttemptRepository, TaskRepository};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use djinn_core::models::TaskDeliveryIdentity;

use crate::direct_delivery::{
    AttemptRef, BoundaryOperation, Candidate, CandidateBuild, CandidateBuilder, DeliveryOutcome,
    DeliverySource, DirectDeliveryEngine, INTEGRATION_RECONCILE_BUDGET, LEGACY_DELIVERY_LABEL,
    PermanentStaleness, RemoteUpdate, RepositoryDeliveryLedger, boundary_operations_scope,
};

/// Which production release seam a case is exercising. Both are driven by every
/// matrix below so neither is proven only by proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoverySeam {
    Zombie,
    ExecutionStateOrphan,
}

impl RecoverySeam {
    const ALL: [RecoverySeam; 2] = [RecoverySeam::Zombie, RecoverySeam::ExecutionStateOrphan];

    /// Dispatch to the actual collaborator the corresponding actor loop calls.
    async fn admit<F, Fut>(
        self,
        db: Database,
        tasks: &TaskRepository,
        task_id: &str,
        reconcile: F,
    ) -> RecoveryReleaseAdmission
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<DeliveryOutcome>>,
    {
        match self {
            RecoverySeam::Zombie => {
                admit_zombie_session_release(db, tasks, task_id, reconcile).await
            }
            RecoverySeam::ExecutionStateOrphan => {
                admit_execution_state_orphan_release(db, tasks, task_id, reconcile).await
            }
        }
    }
}

#[derive(Clone)]
struct CountingRemote(Arc<Mutex<(String, usize)>>);

#[async_trait]
impl AttemptRef for CountingRemote {
    async fn observe(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(Some(self.0.lock().unwrap().0.clone()))
    }
    async fn update_expected_old(
        &self,
        _: &str,
        old: &str,
        new: &str,
    ) -> anyhow::Result<RemoteUpdate> {
        let mut state = self.0.lock().unwrap();
        state.1 += 1;
        if state.0 == old {
            state.0 = new.into();
            Ok(RemoteUpdate::Updated { sha: new.into() })
        } else {
            Ok(RemoteUpdate::Stale {
                observed_sha: Some(state.0.clone()),
            })
        }
    }
}

struct FixedCandidateBuilder;

#[async_trait]
impl CandidateBuilder for FixedCandidateBuilder {
    async fn build(
        &self,
        _: &TaskDeliveryIdentity,
        _: &DeliverySource,
        parent: &str,
    ) -> anyhow::Result<CandidateBuild> {
        Ok(CandidateBuild::Clean(Candidate {
            candidate_sha: "fixture-candidate".into(),
            patch_digest: "fixture-patch".into(),
            selected_parent_sha: parent.into(),
        }))
    }
}

/// Each recovery effect observed at its own production boundary. Reopen/release
/// is counted as a task transition, integration and dependent release as two
/// separately-keyed event counters.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryEffects {
    admission: String,
    engine_runs: usize,
    remote_ref_pushes: usize,
    direct_appends: usize,
    task_pr_operations: usize,
    source_task_updates: usize,
    dependent_releases: usize,
}

fn is_task_pr_operation(op: &BoundaryOperation) -> bool {
    matches!(
        op,
        BoundaryOperation::SupervisorPrOpen
            | BoundaryOperation::TaskPrLookup
            | BoundaryOperation::TaskPrAdopt
            | BoundaryOperation::TaskPrStatusPoll
            | BoundaryOperation::TaskPrReviewPoll
            | BoundaryOperation::TaskPrMergedPoll
            | BoundaryOperation::TaskPrInlineCleanup
            | BoundaryOperation::TaskPrStaleCleanup
            | BoundaryOperation::TaskPrCreate
            | BoundaryOperation::TaskPrMerge
            | BoundaryOperation::TaskPrAutoMerge
            | BoundaryOperation::TaskPrApproval
            | BoundaryOperation::TaskPrSignoff
            | BoundaryOperation::TaskPrCustomEnqueue
            | BoundaryOperation::AttemptPrCreateOrAdoptRequest
    )
}

struct RecoveryFixture {
    db: Database,
    tasks: TaskRepository,
    task_id: String,
    epic_id: String,
    source_updates: Arc<Mutex<usize>>,
    dependent_updates: Arc<Mutex<usize>>,
    events: EventBus,
}

/// Build a recovery fixture whose source task carries `status`.
///
/// The status is a parameter rather than a constant because the two axes this
/// module covers want different ones, and the difference is load-bearing:
///
/// * Admission-only cases use `in_progress` — the status the zombie loop would
///   release to `open`. If a refusal ever regressed into a release, the
///   transition would be real and visible.
/// * Engine-driving cases use `approved`, because that is the only status the
///   delivery contract integrates from: `TaskRepository::task_integrated`
///   closes `WHERE status='approved'` and reports anything else as `Stale`.
///
/// See the module-level note on `DirectDeliveryEngine::integrate` for the hang
/// that distinction used to cause, and for the regression that pins it shut.
async fn recovery_fixture_with_status(status: &str) -> RecoveryFixture {
    let db = Database::open_in_memory().unwrap();
    let source_updates = Arc::new(Mutex::new(0usize));
    let dependent_updates = Arc::new(Mutex::new(0usize));
    let source_slot = Arc::new(Mutex::new(String::new()));
    let dependent_slot = Arc::new(Mutex::new(String::new()));

    let observed_source = source_updates.clone();
    let observed_dependent = dependent_updates.clone();
    let source_for_events = source_slot.clone();
    let dependent_for_events = dependent_slot.clone();
    let events = EventBus::new(move |event| {
        if event.entity_type != "task" || event.action != "updated" {
            return;
        }
        let id = event.payload["task"]["id"].as_str().unwrap_or_default();
        if id == source_for_events.lock().unwrap().as_str() {
            *observed_source.lock().unwrap() += 1;
        } else if id == dependent_for_events.lock().unwrap().as_str() {
            *observed_dependent.lock().unwrap() += 1;
        }
    });
    let epic = EpicRepository::new(db.clone(), EventBus::noop())
        .create("recovery", "", "", "", "", None)
        .await
        .unwrap();
    let tasks = TaskRepository::new(db.clone(), events.clone());
    let task = tasks
        .create(&epic.id, "recovery", "", "", "task", 0, "", Some(status))
        .await
        .unwrap();
    let dependent = tasks
        .create(&epic.id, "dependent", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    tasks.add_blocker(&dependent.id, &task.id).await.unwrap();
    *source_slot.lock().unwrap() = task.id.clone();
    *dependent_slot.lock().unwrap() = dependent.id.clone();

    assert!(
        task.pr_url.is_none(),
        "recovery routing must never have nullable PR data to infer from"
    );

    RecoveryFixture {
        db,
        tasks,
        task_id: task.id,
        epic_id: epic.id,
        source_updates,
        dependent_updates,
        events,
    }
}

/// Admission-only fixture: the task sits in `in_progress`, so a refusal that
/// regressed into a release would show up as a real status transition.
async fn recovery_fixture() -> RecoveryFixture {
    recovery_fixture_with_status("in_progress").await
}

/// Engine-driving fixture: `approved` is the only status
/// `TaskRepository::task_integrated` will close from.
async fn integrable_recovery_fixture() -> RecoveryFixture {
    recovery_fixture_with_status("approved").await
}

impl RecoveryFixture {
    fn engine(
        &self,
        remote: Arc<Mutex<(String, usize)>>,
    ) -> DirectDeliveryEngine<RepositoryDeliveryLedger, CountingRemote, FixedCandidateBuilder> {
        DirectDeliveryEngine::new(
            RepositoryDeliveryLedger::new(
                self.db.clone(),
                ProposalBuildAttemptRepository::new(self.db.clone()),
                TaskRepository::new(self.db.clone(), self.events.clone()),
            ),
            CountingRemote(remote),
            FixedCandidateBuilder,
        )
    }

    async fn seed(&self, state: Option<&str>) {
        djinn_db::test_support::seed_direct_delivery_liveness_fixture_for_test(
            &self.db,
            &self.epic_id,
            &self.task_id,
            state,
        )
        .await;
    }

    async fn status(&self) -> String {
        self.tasks.get(&self.task_id).await.unwrap().unwrap().status
    }
}

// ─── AC1 / AC2: Applying is consumed before any release/reopen decision ────

/// Both seams consume `Applying` through the real engine, and canonical
/// ownership resolution precedes the delivery's own mutations.
#[tokio::test]
async fn both_recovery_seams_consume_applying_before_any_release_decision() {
    for seam in RecoverySeam::ALL {
        let boundary = boundary_operations_scope().await;
        let fixture = integrable_recovery_fixture().await;
        fixture.seed(Some("applying")).await;

        let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
        let engine = fixture.engine(remote.clone());
        let engine_runs = Arc::new(AtomicUsize::new(0));
        let runs = engine_runs.clone();

        *fixture.source_updates.lock().unwrap() = 0;
        *fixture.dependent_updates.lock().unwrap() = 0;
        let checkpoint = boundary.checkpoint();

        let admission = seam
            .admit(fixture.db.clone(), &fixture.tasks, &fixture.task_id, || {
                let engine = &engine;
                let task_id = fixture.task_id.clone();
                let runs = runs.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    crate::dispatch::wave_dispatch::run_direct_completion(|| {
                        engine.deliver(DeliverySource {
                            task_id,
                            delivery_generation: 1,
                            transition_id: "fixture-prepare".into(),
                            source_sha: "fixture-source".into(),
                            normalized_patch: "fixture-patch".into(),
                        })
                    })
                    .await
                }
            })
            .await;
        let operations = boundary.operations_since(checkpoint);
        let remote_ref_pushes = remote.lock().unwrap().1;
        let source_task_updates = *fixture.source_updates.lock().unwrap();
        let dependent_releases = *fixture.dependent_updates.lock().unwrap();
        let effects = RecoveryEffects {
            admission: format!("{admission:?}"),
            engine_runs: engine_runs.load(Ordering::SeqCst),
            remote_ref_pushes,
            direct_appends: operations
                .iter()
                .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
                .count(),
            task_pr_operations: operations
                .iter()
                .filter(|op| is_task_pr_operation(op))
                .count(),
            source_task_updates,
            dependent_releases,
        };

        assert_eq!(
            effects,
            RecoveryEffects {
                admission: "Refuse(Reconciled)".to_owned(),
                engine_runs: 1,
                remote_ref_pushes: 1,
                direct_appends: 1,
                task_pr_operations: 0,
                source_task_updates: 1,
                dependent_releases: 1,
            },
            "{seam:?}: Applying must be consumed by the engine and then refuse release"
        );

        let resolve_at = operations
            .iter()
            .position(|op| matches!(op, BoundaryOperation::ResolveTaskActiveAttempt))
            .expect("the seam must resolve the canonical active attempt");
        let append_at = operations
            .iter()
            .position(|op| matches!(op, BoundaryOperation::DirectAppend))
            .expect("the engine must reach its append boundary");
        assert!(
            resolve_at < append_at,
            "{seam:?}: ResolveTaskActiveAttempt must precede the transition-causing append: {operations:?}"
        );

        // Converged to the exact candidate, and the task is terminal — never
        // released back to `open`.
        let generations = djinn_db::test_support::direct_delivery_generations_for_test(
            &fixture.db,
            &fixture.task_id,
        )
        .await;
        assert_eq!(generations.len(), 1);
        assert_eq!(generations[0].state, "applied");
        assert_eq!(generations[0].candidate_sha, "fixture-candidate");
        assert_eq!(
            fixture.status().await,
            "closed",
            "{seam:?}: the delivery, not the recovery seam, owns the terminal transition"
        );
    }
}

// ─── AC3: independent replay proof per seam ────────────────────────────────

/// Each seam, invoked twice over exact `Applied`, refuses release both times and
/// changes nothing.
#[tokio::test]
async fn each_recovery_seam_replays_exact_applied_without_release_or_second_effect() {
    for seam in RecoverySeam::ALL {
        let boundary = boundary_operations_scope().await;
        let fixture = integrable_recovery_fixture().await;
        fixture.seed(Some("applying")).await;

        let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
        let engine = fixture.engine(remote.clone());

        // Reach exact Applied + closed through the real engine and the real
        // TaskIntegrated transition, before either seam is entered.
        let settled = crate::dispatch::wave_dispatch::run_direct_completion(|| {
            engine.deliver(DeliverySource {
                task_id: fixture.task_id.clone(),
                delivery_generation: 1,
                transition_id: "fixture-prepare".into(),
                source_sha: "fixture-source".into(),
                normalized_patch: "fixture-patch".into(),
            })
        })
        .await
        .expect("engine settles the fixture generation");
        assert!(
            matches!(settled, DeliveryOutcome::Integrated { .. }),
            "{seam:?}: engine did not integrate: {settled:?}"
        );

        let generations_before = djinn_db::test_support::direct_delivery_generations_for_test(
            &fixture.db,
            &fixture.task_id,
        )
        .await;
        let cardinality_before =
            djinn_db::test_support::direct_delivery_candidate_cardinality_for_test(
                &fixture.db,
                &fixture.task_id,
            )
            .await;
        let pushes_before = remote.lock().unwrap().1;
        let status_before = fixture.status().await;
        assert_eq!(status_before, "closed");

        let engine_runs = Arc::new(AtomicUsize::new(0));

        for call in 1..=2 {
            *fixture.source_updates.lock().unwrap() = 0;
            *fixture.dependent_updates.lock().unwrap() = 0;
            let checkpoint = boundary.checkpoint();
            let runs = engine_runs.clone();

            let admission = seam
                .admit(
                    fixture.db.clone(),
                    &fixture.tasks,
                    &fixture.task_id,
                    || async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        panic!("exact Applied must never re-enter the delivery engine")
                    },
                )
                .await;

            let operations = boundary.operations_since(checkpoint);
            let remote_ref_pushes = remote.lock().unwrap().1 - pushes_before;
            let source_task_updates = *fixture.source_updates.lock().unwrap();
            let dependent_releases = *fixture.dependent_updates.lock().unwrap();
            let effects = RecoveryEffects {
                admission: format!("{admission:?}"),
                engine_runs: engine_runs.load(Ordering::SeqCst),
                remote_ref_pushes,
                direct_appends: operations
                    .iter()
                    .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
                    .count(),
                task_pr_operations: operations
                    .iter()
                    .filter(|op| is_task_pr_operation(op))
                    .count(),
                source_task_updates,
                dependent_releases,
            };
            assert_eq!(
                effects,
                RecoveryEffects {
                    admission: "Refuse(Settled)".to_owned(),
                    engine_runs: 0,
                    remote_ref_pushes: 0,
                    direct_appends: 0,
                    task_pr_operations: 0,
                    source_task_updates: 0,
                    dependent_releases: 0,
                },
                "{seam:?} call {call}: a settled generation must produce no recovery effect"
            );
            assert_eq!(
                fixture.status().await,
                "closed",
                "{seam:?} call {call}: the task must stay terminal, never reopened"
            );
            assert_eq!(
                djinn_db::test_support::direct_delivery_generations_for_test(
                    &fixture.db,
                    &fixture.task_id
                )
                .await,
                generations_before,
                "{seam:?} call {call}: the immutable generation must be unchanged"
            );
            assert_eq!(
                djinn_db::test_support::direct_delivery_candidate_cardinality_for_test(
                    &fixture.db,
                    &fixture.task_id
                )
                .await,
                cardinality_before,
                "{seam:?} call {call}: candidate cardinality must not grow"
            );
        }
    }
}

/// `Conflict`, and `Applied` + closed with no `pr_url`, both refuse release at
/// both seams without touching anything.
#[tokio::test]
async fn both_recovery_seams_refuse_conflict_and_applied_closed_without_pr_url() {
    for seam in RecoverySeam::ALL {
        for state in ["conflict", "applied"] {
            let fixture = recovery_fixture().await;
            fixture.seed(Some(state)).await;
            if state == "applied" {
                fixture
                    .tasks
                    .set_status(&fixture.task_id, "closed")
                    .await
                    .unwrap();
            }

            let task = fixture.tasks.get(&fixture.task_id).await.unwrap().unwrap();
            assert!(
                task.pr_url.is_none(),
                "{seam:?}/{state}: the refusal must not need a PR URL"
            );
            let status_before = task.status;
            let generations_before = djinn_db::test_support::direct_delivery_generations_for_test(
                &fixture.db,
                &fixture.task_id,
            )
            .await;
            *fixture.source_updates.lock().unwrap() = 0;

            let admission = seam
                .admit(
                    fixture.db.clone(),
                    &fixture.tasks,
                    &fixture.task_id,
                    || async { panic!("{state} must not enter the delivery engine") },
                )
                .await;

            assert_eq!(
                admission,
                RecoveryReleaseAdmission::Refuse(RecoveryReleaseRefusal::Settled),
                "{seam:?}/{state}: must refuse release"
            );
            assert_eq!(
                *fixture.source_updates.lock().unwrap(),
                0,
                "{seam:?}/{state}: refusing must not mutate the task"
            );
            assert_eq!(fixture.status().await, status_before);
            assert_eq!(
                djinn_db::test_support::direct_delivery_generations_for_test(
                    &fixture.db,
                    &fixture.task_id
                )
                .await,
                generations_before
            );
        }
    }
}

// ─── AC4: fail-closed and retained-legacy at both seams ───────────────────

#[derive(Clone, Copy, Debug)]
enum RecoveryRoutingCase {
    UnresolvedOwnership,
    MissingSchema,
    MissingEpoch,
    UnknownEpoch,
    UnknownDeliveryState,
    SupportedDisabled,
    SupportedActiveExplicitLegacy,
}

/// Fail-closed and retained-legacy routing, asserted at **both** recovery seams.
///
/// The retained-legacy half is a positive assertion: those cases must still
/// return `Release`, i.e. the seam's pre-existing recovery transition is
/// preserved rather than merely "not crashing".
#[tokio::test]
async fn both_recovery_seams_fail_closed_and_preserve_legacy_release_by_persisted_state() {
    for seam in RecoverySeam::ALL {
        for case in [
            RecoveryRoutingCase::UnresolvedOwnership,
            RecoveryRoutingCase::MissingSchema,
            RecoveryRoutingCase::MissingEpoch,
            RecoveryRoutingCase::UnknownEpoch,
            RecoveryRoutingCase::UnknownDeliveryState,
            RecoveryRoutingCase::SupportedDisabled,
            RecoveryRoutingCase::SupportedActiveExplicitLegacy,
        ] {
            let fixture = recovery_fixture().await;

            match case {
                RecoveryRoutingCase::UnresolvedOwnership => {
                    djinn_db::test_support::activate_direct_delivery_epoch_for_test(&fixture.db)
                        .await;
                    djinn_db::test_support::seed_direct_delivery_proposal_for_test(
                        &fixture.db,
                        &fixture.task_id,
                        &fixture.task_id[..8],
                    )
                    .await;
                }
                RecoveryRoutingCase::MissingSchema => {
                    fixture.seed(Some("applying")).await;
                    djinn_db::test_support::drop_table_cascade_for_test(
                        &fixture.db,
                        "task_deliveries",
                    )
                    .await;
                }
                RecoveryRoutingCase::MissingEpoch => {
                    fixture.seed(Some("applying")).await;
                    djinn_db::test_support::remove_direct_delivery_epoch_for_test(&fixture.db)
                        .await;
                }
                RecoveryRoutingCase::UnknownEpoch => {
                    fixture.seed(Some("applying")).await;
                    djinn_db::test_support::seed_unknown_direct_delivery_epoch_for_test(
                        &fixture.db,
                    )
                    .await;
                }
                RecoveryRoutingCase::UnknownDeliveryState => {
                    fixture.seed(Some("applying")).await;
                    djinn_db::test_support::seed_unknown_task_delivery_state_for_test(
                        &fixture.db,
                        &fixture.task_id,
                        "quiesced",
                    )
                    .await;
                }
                RecoveryRoutingCase::SupportedDisabled => {
                    fixture.seed(Some("applying")).await;
                    djinn_db::test_support::disable_direct_delivery_epoch_for_test(&fixture.db)
                        .await;
                }
                RecoveryRoutingCase::SupportedActiveExplicitLegacy => {
                    fixture.seed(Some("applying")).await;
                    fixture
                        .tasks
                        .update_labels(&fixture.task_id, &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#))
                        .await
                        .unwrap();
                }
            }

            let engine_runs = Arc::new(AtomicUsize::new(0));
            let runs = engine_runs.clone();
            *fixture.source_updates.lock().unwrap() = 0;
            let status_before = fixture.status().await;

            let admission = seam
                .admit(
                    fixture.db.clone(),
                    &fixture.tasks,
                    &fixture.task_id,
                    || async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        panic!("{case:?} must never reach the delivery engine")
                    },
                )
                .await;

            match case {
                RecoveryRoutingCase::UnresolvedOwnership
                | RecoveryRoutingCase::MissingSchema
                | RecoveryRoutingCase::MissingEpoch
                | RecoveryRoutingCase::UnknownEpoch
                | RecoveryRoutingCase::UnknownDeliveryState => {
                    assert_eq!(
                        admission,
                        RecoveryReleaseAdmission::Refuse(RecoveryReleaseRefusal::FailedClosed),
                        "{seam:?}/{case:?}: must fail closed, distinctly from a settled delivery"
                    );
                }
                RecoveryRoutingCase::SupportedDisabled
                | RecoveryRoutingCase::SupportedActiveExplicitLegacy => {
                    assert_eq!(
                        admission,
                        RecoveryReleaseAdmission::Release,
                        "{seam:?}/{case:?}: retained legacy must preserve the existing recovery release"
                    );
                }
            }

            assert_eq!(
                engine_runs.load(Ordering::SeqCst),
                0,
                "{seam:?}/{case:?}: no case here may run the direct engine"
            );

            // The only mutation any case here may perform is the epoch
            // boundary's own fail-closed park, and only for the cases that
            // reach it. Everything else must leave the task exactly as found —
            // in particular, none of them may perform the seam's *release*,
            // which from `in_progress` would move the task to `open`.
            let parks = *fixture.source_updates.lock().unwrap();
            let status_after = fixture.status().await;
            match case {
                // Unresolvable ownership and unreadable schema/epoch are parked
                // by the shared admission boundary before this seam sees them.
                RecoveryRoutingCase::UnresolvedOwnership
                | RecoveryRoutingCase::MissingSchema
                | RecoveryRoutingCase::MissingEpoch
                | RecoveryRoutingCase::UnknownEpoch => {
                    assert_eq!(
                        (parks, status_after.as_str()),
                        (1, "needs_lead_intervention"),
                        "{seam:?}/{case:?}: failing closed must park exactly once, never release"
                    );
                }
                // An undefined persisted delivery state aborts before any
                // mutation at all, park included.
                RecoveryRoutingCase::UnknownDeliveryState => {
                    assert_eq!(
                        (parks, status_after.as_str()),
                        (0, status_before.as_str()),
                        "{seam:?}/{case:?}: an unreadable ledger row must not move the task"
                    );
                }
                // Retained legacy hands the seam its ordinary release decision;
                // admission itself changes nothing.
                RecoveryRoutingCase::SupportedDisabled
                | RecoveryRoutingCase::SupportedActiveExplicitLegacy => {
                    assert_eq!(
                        (parks, status_after.as_str()),
                        (0, status_before.as_str()),
                        "{seam:?}/{case:?}: admitting a legacy release must not itself mutate"
                    );
                }
            }
            assert_ne!(
                status_after, "open",
                "{seam:?}/{case:?}: admission must never perform the seam's release"
            );
        }
    }
}

// ─── i5fn: a permanently stale generation must terminate, not spin ─────────

/// The production hang this module previously had to work around.
///
/// Both recovery loops select `in_progress`, `in_task_review`, and
/// `in_lead_intervention` — never `approved` — so every `Applying` generation
/// they hand the engine is permanently stale at
/// `TaskRepository::task_integrated`, which closes only `WHERE
/// status='approved'`. Before i5fn that condition was reported as the same
/// undifferentiated `Stale` as an unfinalized parent head, and
/// `DirectDeliveryEngine::integrate` retried it in an unbounded 1 ms loop, so
/// the coordinator's recovery pass never returned.
///
/// Terminating is not enough to pass here. The engine must terminate *because
/// it recognised the condition*, so this asserts the typed outcome and that the
/// seam returned in a fraction of the transient budget — a fix that merely
/// capped the retry would spend the whole budget and land on the wrong variant.
#[tokio::test]
async fn both_recovery_seams_terminate_on_a_permanently_stale_generation() {
    for seam in RecoverySeam::ALL {
        // `in_progress` is exactly what the zombie and orphan loops select.
        let fixture = recovery_fixture().await;
        fixture.seed(Some("applying")).await;

        let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
        let engine = fixture.engine(remote.clone());
        let observed_outcome = Arc::new(Mutex::new(None));
        let recorded = observed_outcome.clone();

        let started = std::time::Instant::now();
        let admission = tokio::time::timeout(
            INTEGRATION_RECONCILE_BUDGET * 3,
            seam.admit(fixture.db.clone(), &fixture.tasks, &fixture.task_id, || {
                let engine = &engine;
                let task_id = fixture.task_id.clone();
                let recorded = recorded.clone();
                async move {
                    let outcome = crate::dispatch::wave_dispatch::run_direct_completion(|| {
                        engine.deliver(DeliverySource {
                            task_id,
                            delivery_generation: 1,
                            transition_id: "fixture-prepare".into(),
                            source_sha: "fixture-source".into(),
                            normalized_patch: "fixture-patch".into(),
                        })
                    })
                    .await;
                    if let Ok(outcome) = &outcome {
                        *recorded.lock().unwrap() = Some(outcome.clone());
                    }
                    outcome
                }
            }),
        )
        .await
        .unwrap_or_else(|_| {
            panic!("{seam:?}: a permanently stale generation hung the recovery seam")
        });
        let elapsed = started.elapsed();

        // The engine names the condition rather than discovering it by running
        // out of patience.
        assert_eq!(
            observed_outcome.lock().unwrap().clone(),
            Some(DeliveryOutcome::Unintegrable {
                candidate_sha: "fixture-candidate".to_owned(),
                reason: PermanentStaleness::TaskNotApproved,
            }),
            "{seam:?}: the engine must report why this generation can never integrate"
        );
        assert!(
            elapsed < INTEGRATION_RECONCILE_BUDGET,
            "{seam:?}: a permanent condition must not consume the transient wait budget (took {elapsed:?})"
        );
        assert_eq!(
            format!("{admission:?}"),
            "Refuse(ReconcileFailed)".to_owned(),
            "{seam:?}: an unintegrable generation is not a reconciliation"
        );

        // The seam still refuses the release, and nothing guessed the task's
        // next state on its behalf.
        assert_eq!(
            fixture.status().await,
            "in_progress",
            "{seam:?}: the recovery seam must not release a task it refused"
        );
        let generations = djinn_db::test_support::direct_delivery_generations_for_test(
            &fixture.db,
            &fixture.task_id,
        )
        .await;
        assert_eq!(generations.len(), 1);
        assert_eq!(
            generations[0].state, "applying",
            "{seam:?}: an unintegrable generation stays exactly as persisted"
        );
    }
}
