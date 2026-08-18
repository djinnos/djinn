//! Repository-backed direct-delivery liveness for the arbiter second-strike
//! retry seam, plus the closing four-surface audit (`super::*` = `retry`).
//!
//! The first half completes the missing retry proof: every test enters
//! `admit_second_strike_retry`, the collaborator
//! `CoordinatorActor::dispatch_arbiter_second_strike` itself calls.
//!
//! The second half is the cross-surface audit this slice owes. It names and
//! invokes **all four** production surfaces delivered across `v8gs`/`zq6e`,
//! `rukl`, `obc4`, and this task — ready dispatch, the respawn guard, both
//! recovery-release seams, and second-strike retry — twice each over exact
//! `Applied`, and drives every fail-closed and retained-legacy persisted state
//! through each of them. No assertion in the audit targets a shared admission
//! helper; each surface is entered by its own production entry point.

use super::*;
use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{Database, EpicRepository, ProposalBuildAttemptRepository, TaskRepository};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use djinn_core::models::TaskDeliveryIdentity;

use crate::direct_delivery::{
    AttemptRef, BoundaryOperation, Candidate, CandidateBuild, CandidateBuilder, DeliveryOutcome,
    DeliverySource, DirectDeliveryEngine, LEGACY_DELIVERY_LABEL, RemoteUpdate,
    RepositoryDeliveryLedger, boundary_operations_scope,
};
use crate::dispatch::respawn_guard::{
    PrReworkSignal, RespawnGuardDecision, run_respawn_guard_with_reconciler,
};
use crate::dispatch::session_recovery::{
    RecoveryReleaseRefusal, admit_execution_state_orphan_release, admit_zombie_session_release,
};
use crate::dispatch::task_dispatch::{ReadyDispatchContinuation, continue_ready_dispatch};

// ─── shared fixture plumbing ───────────────────────────────────────────────

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

struct SurfaceFixture {
    db: Database,
    tasks: TaskRepository,
    task_id: String,
    epic_id: String,
    source_updates: Arc<Mutex<usize>>,
    dependent_updates: Arc<Mutex<usize>>,
    events: EventBus,
}

async fn surface_fixture_with_status(status: &str) -> SurfaceFixture {
    surface_fixture_in(Database::open_in_memory().unwrap(), status).await
}

/// Build one surface's fixture inside an **existing** database.
///
/// Everything this creates — epic, task, dependent, event bus — is
/// task-scoped, so a matrix whose cells vary only task-scoped state can hoist
/// the `CREATE DATABASE … TEMPLATE` clone out of its inner loop and share one
/// database across those cells. Two fixtures built on the same database share
/// nothing but the database.
async fn surface_fixture_in(db: Database, status: &str) -> SurfaceFixture {
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
        .create("surface", "", "", "", "", None)
        .await
        .unwrap();
    let tasks = TaskRepository::new(db.clone(), events.clone());
    let task = tasks
        .create(&epic.id, "surface", "", "", "task", 0, "", Some(status))
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
        "no surface may infer routing from nullable PR data"
    );

    SurfaceFixture {
        db,
        tasks,
        task_id: task.id,
        epic_id: epic.id,
        source_updates,
        dependent_updates,
        events,
    }
}

/// `approved` is the only status `TaskRepository::task_integrated` closes from,
/// and therefore the only one an engine-driving fixture can use. See the note in
/// `session_recovery_direct_delivery_tests` on the unbounded `Stale` retry.
async fn surface_fixture() -> SurfaceFixture {
    surface_fixture_with_status("approved").await
}

impl SurfaceFixture {
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

    /// Drive the real engine to exact `Applied` + closed through real
    /// `TaskIntegrated`, before any surface is entered.
    async fn settle(&self, remote: Arc<Mutex<(String, usize)>>) {
        let engine = self.engine(remote);
        let outcome = crate::dispatch::wave_dispatch::run_direct_completion(|| {
            engine.deliver(DeliverySource {
                task_id: self.task_id.clone(),
                delivery_generation: 1,
                transition_id: "fixture-prepare".into(),
                source_sha: "fixture-source".into(),
                normalized_patch: "fixture-patch".into(),
            })
        })
        .await
        .expect("engine settles the fixture generation");
        assert!(
            matches!(outcome, DeliveryOutcome::Integrated { .. }),
            "fixture must integrate before replay, got {outcome:?}"
        );
    }
}

// ─── AC1 / AC2: the second-strike retry seam ───────────────────────────────

/// Effects one second-strike admission may produce.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RetryEffects {
    admission: String,
    engine_runs: usize,
    remote_ref_pushes: usize,
    direct_appends: usize,
    task_pr_operations: usize,
    source_task_updates: usize,
    dependent_releases: usize,
}

/// The retry seam consumes `Applying` through the real engine, and canonical
/// ownership resolution precedes the delivery's own mutations.
#[tokio::test]
async fn second_strike_seam_consumes_applying_before_retry_mutation() {
    let boundary = boundary_operations_scope().await;
    let fixture = surface_fixture().await;
    fixture.seed(Some("applying")).await;

    let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
    let engine = fixture.engine(remote.clone());
    let engine_runs = Arc::new(AtomicUsize::new(0));
    let runs = engine_runs.clone();

    *fixture.source_updates.lock().unwrap() = 0;
    *fixture.dependent_updates.lock().unwrap() = 0;
    let checkpoint = boundary.checkpoint();

    let admission =
        admit_second_strike_retry(fixture.db.clone(), &fixture.tasks, &fixture.task_id, || {
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
    let effects = RetryEffects {
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
        RetryEffects {
            admission: "Refuse(Reconciled)".to_owned(),
            engine_runs: 1,
            remote_ref_pushes: 1,
            direct_appends: 1,
            task_pr_operations: 0,
            source_task_updates: 1,
            dependent_releases: 1,
        },
        "Applying must be consumed by the engine and then refuse retry escalation"
    );

    let resolve_at = operations
        .iter()
        .position(|op| matches!(op, BoundaryOperation::ResolveTaskActiveAttempt))
        .expect("the retry seam must resolve the canonical active attempt");
    let append_at = operations
        .iter()
        .position(|op| matches!(op, BoundaryOperation::DirectAppend))
        .expect("the engine must reach its append boundary");
    assert!(
        resolve_at < append_at,
        "ResolveTaskActiveAttempt must precede any retry-causing effect: {operations:?}"
    );
    assert!(
        operations
            .first()
            .is_some_and(|op| matches!(op, BoundaryOperation::CapabilityProbe)),
        "the epoch capability probe must come first: {operations:?}"
    );
    assert_eq!(fixture.status().await, "closed");
}

/// `Conflict`, and `Applied` + closed with no `pr_url`, refuse retry outright.
#[tokio::test]
async fn second_strike_seam_refuses_conflict_and_applied_closed_without_pr_url() {
    for state in ["conflict", "applied"] {
        let fixture = surface_fixture().await;
        fixture.seed(Some(state)).await;
        if state == "applied" {
            fixture
                .tasks
                .set_status(&fixture.task_id, "closed")
                .await
                .unwrap();
        }
        let task = fixture.tasks.get(&fixture.task_id).await.unwrap().unwrap();
        assert!(task.pr_url.is_none(), "{state}: refusal must not need a PR");
        let status_before = task.status;
        *fixture.source_updates.lock().unwrap() = 0;

        let admission = admit_second_strike_retry(
            fixture.db.clone(),
            &fixture.tasks,
            &fixture.task_id,
            || async { panic!("{state} must not enter the delivery engine") },
        )
        .await;

        assert_eq!(
            admission,
            RecoveryReleaseAdmission::Refuse(RecoveryReleaseRefusal::Settled),
            "{state}: must refuse retry escalation"
        );
        assert_eq!(*fixture.source_updates.lock().unwrap(), 0);
        assert_eq!(fixture.status().await, status_before);
    }
}

// ─── the four-surface audit ────────────────────────────────────────────────

/// The four production surfaces this epic's liveness cutover has to hold at.
/// Each variant is entered by its own production entry point; none is a shared
/// admission helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    ReadyDispatch,
    RespawnGuard,
    ZombieRelease,
    OrphanRelease,
    SecondStrikeRetry,
}

impl Surface {
    const ALL: [Surface; 5] = [
        Surface::ReadyDispatch,
        Surface::RespawnGuard,
        Surface::ZombieRelease,
        Surface::OrphanRelease,
        Surface::SecondStrikeRetry,
    ];
}

/// What one surface invocation did, normalized across the five entry points so
/// the audit can compare them uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SurfaceOutcome {
    /// Did the surface hand control to its legacy continuation / release /
    /// retry, i.e. would a spawn, reopen, or escalation have followed?
    proceeded_to_legacy: bool,
    /// Did the surface fail closed rather than reaching a decision?
    failed_closed: bool,
    engine_runs: usize,
}

/// Invoke one surface through its own production entry point.
///
/// `reconcile` is the caller-owned direct-delivery engine every one of these
/// surfaces takes; `pr_url` is threaded through so the respawn guard's adoption
/// step stays reachable and is therefore observable as a legacy effect.
async fn invoke_surface<F, Fut>(
    surface: Surface,
    fixture: &SurfaceFixture,
    pr_url: Option<&str>,
    reconcile: F,
) -> anyhow::Result<SurfaceOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<DeliveryOutcome>>,
{
    let engine_runs = Arc::new(AtomicUsize::new(0));
    let counted_runs = engine_runs.clone();
    let counted = move || {
        let runs = counted_runs.clone();
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            reconcile().await
        }
    };

    let (proceeded_to_legacy, failed_closed) = match surface {
        Surface::ReadyDispatch => {
            let spawned = Arc::new(AtomicUsize::new(0));
            let spawned_for_call = spawned.clone();
            let decision = continue_ready_dispatch(
                fixture.db.clone(),
                &fixture.tasks,
                &fixture.task_id,
                counted,
                || async move {
                    spawned_for_call.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
            match decision {
                Ok(ReadyDispatchContinuation::LegacyDispatch(())) => {
                    assert_eq!(spawned.load(Ordering::SeqCst), 1);
                    (true, false)
                }
                Ok(ReadyDispatchContinuation::Parked) => (false, true),
                Ok(_) => (false, false),
                Err(_) => (false, true),
            }
        }
        Surface::RespawnGuard => {
            let decision = run_respawn_guard_with_reconciler(
                &fixture.db,
                &fixture.task_id,
                "worker",
                pr_url,
                None::<PrReworkSignal>,
                counted,
            )
            .await;
            match decision {
                // Adoption is the guard's legacy PR effect.
                RespawnGuardDecision::Adopted { .. } | RespawnGuardDecision::Allow => (true, false),
                RespawnGuardDecision::Defer(_) => (false, false),
            }
        }
        Surface::ZombieRelease | Surface::OrphanRelease | Surface::SecondStrikeRetry => {
            let admission = match surface {
                Surface::ZombieRelease => {
                    admit_zombie_session_release(
                        fixture.db.clone(),
                        &fixture.tasks,
                        &fixture.task_id,
                        counted,
                    )
                    .await
                }
                Surface::OrphanRelease => {
                    admit_execution_state_orphan_release(
                        fixture.db.clone(),
                        &fixture.tasks,
                        &fixture.task_id,
                        counted,
                    )
                    .await
                }
                _ => {
                    admit_second_strike_retry(
                        fixture.db.clone(),
                        &fixture.tasks,
                        &fixture.task_id,
                        counted,
                    )
                    .await
                }
            };
            match admission {
                RecoveryReleaseAdmission::Release => (true, false),
                RecoveryReleaseAdmission::Refuse(RecoveryReleaseRefusal::FailedClosed) => {
                    (false, true)
                }
                RecoveryReleaseAdmission::Refuse(_) => (false, false),
            }
        }
    };

    Ok(SurfaceOutcome {
        proceeded_to_legacy,
        failed_closed,
        engine_runs: engine_runs.load(Ordering::SeqCst),
    })
}

/// **Every** surface, invoked twice over exact `Applied`, produces no effect.
///
/// This is the audit's core claim: the replay guarantee is not a property of one
/// well-covered surface, it holds at all four (five entry points) simultaneously.
#[tokio::test]
async fn every_surface_replays_exact_applied_twice_without_effect() {
    for surface in Surface::ALL {
        let boundary = boundary_operations_scope().await;
        let fixture = surface_fixture().await;
        fixture.seed(Some("applying")).await;

        let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
        fixture.settle(remote.clone()).await;

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
        assert_eq!(
            pushes_before, 1,
            "{surface:?}: the pre-audit settle pushed once"
        );
        assert_eq!(fixture.status().await, "closed");

        for call in 1..=2 {
            *fixture.source_updates.lock().unwrap() = 0;
            *fixture.dependent_updates.lock().unwrap() = 0;
            let checkpoint = boundary.checkpoint();

            let outcome = invoke_surface(surface, &fixture, None, || async {
                panic!("{surface:?}: exact Applied must never re-enter the delivery engine")
            })
            .await
            .unwrap();

            let operations = boundary.operations_since(checkpoint);
            assert_eq!(
                outcome,
                SurfaceOutcome {
                    proceeded_to_legacy: false,
                    failed_closed: false,
                    engine_runs: 0,
                },
                "{surface:?} call {call}: a settled generation must not spawn, reopen, retry, \
                 or re-enter the engine"
            );
            assert_eq!(
                operations
                    .iter()
                    .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
                    .count(),
                0,
                "{surface:?} call {call}: no second append"
            );
            assert_eq!(
                operations
                    .iter()
                    .filter(|op| is_task_pr_operation(op))
                    .count(),
                0,
                "{surface:?} call {call}: no task-PR effect"
            );
            assert_eq!(
                remote.lock().unwrap().1 - pushes_before,
                0,
                "{surface:?} call {call}: no second remote push"
            );
            assert_eq!(
                *fixture.source_updates.lock().unwrap(),
                0,
                "{surface:?} call {call}: no repeat integration"
            );
            assert_eq!(
                *fixture.dependent_updates.lock().unwrap(),
                0,
                "{surface:?} call {call}: no repeat dependent release"
            );
            assert_eq!(
                fixture.status().await,
                "closed",
                "{surface:?} call {call}: closed status must survive"
            );
            assert_eq!(
                djinn_db::test_support::direct_delivery_generations_for_test(
                    &fixture.db,
                    &fixture.task_id
                )
                .await,
                generations_before,
                "{surface:?} call {call}: immutable generation unchanged"
            );
            assert_eq!(
                djinn_db::test_support::direct_delivery_candidate_cardinality_for_test(
                    &fixture.db,
                    &fixture.task_id
                )
                .await,
                cardinality_before,
                "{surface:?} call {call}: candidate cardinality unchanged"
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum AuditCase {
    UnresolvedOwnership,
    MissingSchema,
    MissingEpoch,
    UnknownEpoch,
    UnknownDeliveryState,
    SupportedDisabled,
    SupportedActiveExplicitLegacy,
}

/// Every fail-closed and retained-legacy persisted state, driven through every
/// surface.
///
/// Fail-closed cases must never reach a surface's legacy continuation; the two
/// retained-legacy cases must positively reach it at every surface, so "safe"
/// cannot be achieved by refusing everything.
///
/// # One template clone per case, not one per cell
///
/// The matrix is seven persisted states by five surfaces. Every one of those
/// 35 cells used to call `surface_fixture()`, i.e. take its own
/// `CREATE DATABASE … TEMPLATE` clone plus the synchronous `DROP DATABASE` the
/// handle issues on drop — 70 whole-database operations in a single test.
/// Postgres forces a checkpoint on either side of a template clone and those
/// checkpoints are cluster-wide, so under full-suite `cargo nextest`
/// parallelism this test spent nearly all of its wall clock queued behind
/// every other suite's clones: ~9 s alone, past the repository's 90 s
/// `slow-timeout.terminate-after` cap under load.
///
/// The clone is hoisted to the **case** loop instead. Cases genuinely cannot
/// share one: each is a *database-global* corruption — a dropped
/// `task_deliveries` table, a deleted, disabled, or unknown-state epoch row —
/// and `MissingSchema` is not even reversible. Surfaces within a case can
/// share one: each surface gets its own epic, proposal, build attempt, task,
/// and dependent, and every surface entry point below reads and writes only
/// the task it is handed. 35 clones become 7 and all 35 cells still run.
#[tokio::test]
async fn every_surface_fails_closed_and_retains_legacy_across_all_persisted_states() {
    const PR: &str = "https://example.test/pr/audit";

    for case in [
        AuditCase::UnresolvedOwnership,
        AuditCase::MissingSchema,
        AuditCase::MissingEpoch,
        AuditCase::UnknownEpoch,
        AuditCase::UnknownDeliveryState,
        AuditCase::SupportedDisabled,
        AuditCase::SupportedActiveExplicitLegacy,
    ] {
        let db = Database::open_in_memory().unwrap();
        let mut fixtures = Vec::with_capacity(Surface::ALL.len());
        for _ in Surface::ALL {
            fixtures.push(surface_fixture_in(db.clone(), "approved").await);
        }

        // Task-scoped seeding: one independent delivery identity per surface.
        for fixture in &fixtures {
            match case {
                AuditCase::UnresolvedOwnership => {
                    // The proposal exists but no epic carries it, so canonical
                    // ownership cannot resolve. The short id comes from the
                    // random tail of the task UUID because short ids are
                    // unique database-wide and these five tasks share one.
                    djinn_db::test_support::seed_direct_delivery_proposal_for_test(
                        &fixture.db,
                        &fixture.task_id,
                        &fixture.task_id[fixture.task_id.len() - 8..],
                    )
                    .await;
                }
                _ => fixture.seed(Some("applying")).await,
            }

            match case {
                AuditCase::UnknownDeliveryState => {
                    djinn_db::test_support::seed_unknown_task_delivery_state_for_test(
                        &fixture.db,
                        &fixture.task_id,
                        "quiesced",
                    )
                    .await;
                }
                AuditCase::SupportedActiveExplicitLegacy => {
                    fixture
                        .tasks
                        .update_labels(&fixture.task_id, &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#))
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }

        // Database-global state, applied once after every surface is seeded.
        // This half of the case is exactly why a case owns a clone of its own.
        match case {
            AuditCase::UnresolvedOwnership => {
                djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
            }
            AuditCase::MissingSchema => {
                djinn_db::test_support::drop_table_cascade_for_test(&db, "task_deliveries").await;
            }
            AuditCase::MissingEpoch => {
                djinn_db::test_support::remove_direct_delivery_epoch_for_test(&db).await;
            }
            AuditCase::UnknownEpoch => {
                djinn_db::test_support::seed_unknown_direct_delivery_epoch_for_test(&db).await;
            }
            AuditCase::SupportedDisabled => {
                djinn_db::test_support::disable_direct_delivery_epoch_for_test(&db).await;
            }
            AuditCase::UnknownDeliveryState | AuditCase::SupportedActiveExplicitLegacy => {}
        }

        for (surface, fixture) in Surface::ALL.into_iter().zip(&fixtures) {
            let outcome = invoke_surface(surface, fixture, Some(PR), || async {
                panic!("{surface:?}/{case:?} must never reach the delivery engine")
            })
            .await
            .unwrap();

            assert_eq!(
                outcome.engine_runs, 0,
                "{surface:?}/{case:?}: no case here may run the direct engine"
            );

            match case {
                AuditCase::UnresolvedOwnership
                | AuditCase::MissingSchema
                | AuditCase::MissingEpoch
                | AuditCase::UnknownEpoch
                | AuditCase::UnknownDeliveryState => {
                    assert!(
                        !outcome.proceeded_to_legacy,
                        "{surface:?}/{case:?}: must fail closed before any spawn, reopen, \
                         adoption, or retry effect"
                    );
                }
                AuditCase::SupportedDisabled | AuditCase::SupportedActiveExplicitLegacy => {
                    assert!(
                        outcome.proceeded_to_legacy,
                        "{surface:?}/{case:?}: retained legacy must positively reach this \
                         surface's existing continuation"
                    );
                    assert!(
                        !outcome.failed_closed,
                        "{surface:?}/{case:?}: retained legacy must not fail closed"
                    );
                }
            }
        }
    }
}
