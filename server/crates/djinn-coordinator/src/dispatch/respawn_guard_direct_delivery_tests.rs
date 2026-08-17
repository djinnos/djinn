//! Repository-backed direct-delivery liveness for the respawn guard
//! (`super::*` = `respawn_guard`).
//!
//! Every test here enters `run_respawn_guard_with_reconciler` — the seam the
//! production ready-dispatch call site in `task_dispatch.rs` invokes. Nothing
//! asserts on `admit_respawn_guard_liveness` or
//! `admit_direct_delivery_liveness` directly, because classifying correctly and
//! then adopting a PR anyway is exactly the regression this coverage exists to
//! catch.
//!
//! Safety is proven *at this seam* rather than inferred from ready-dispatch
//! coverage: the guard runs outside ready dispatch too, so its own fail-closed
//! and replay matrices are local.

use super::*;
use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{
    Database, EpicRepository, ProposalBuildAttemptRepository, TaskAttemptRepository, TaskRepository,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use djinn_core::models::TaskDeliveryIdentity;

use crate::direct_delivery::{
    AttemptRef, BoundaryOperation, Candidate, CandidateBuild, CandidateBuilder, DeliveryOutcome,
    DeliverySource, DirectDeliveryEngine, LEGACY_DELIVERY_LABEL, RemoteUpdate,
    RepositoryDeliveryLedger, boundary_operations_scope,
};

/// A remote that records every ref update it is asked to perform.
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

/// Everything the guard could do to the world on one invocation, each observed
/// at its own production boundary rather than summed into one total.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GuardEffects {
    decision: String,
    engine_runs: usize,
    remote_ref_pushes: usize,
    task_pr_operations: usize,
    direct_appends: usize,
    source_task_updates: usize,
    dependent_releases: usize,
    guard_attempt_rows: i64,
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

/// One fixture: an epic, a source task, a dependent blocked on it, and event
/// counters bound to each task id separately.
struct GuardFixture {
    db: Database,
    tasks: TaskRepository,
    task_id: String,
    source_updates: Arc<Mutex<usize>>,
    dependent_updates: Arc<Mutex<usize>>,
    events: EventBus,
}

async fn guard_fixture() -> GuardFixture {
    let db = Database::open_in_memory().unwrap();
    let source_updates = Arc::new(Mutex::new(0usize));
    let dependent_updates = Arc::new(Mutex::new(0usize));
    let source_id_slot = Arc::new(Mutex::new(String::new()));
    let dependent_id_slot = Arc::new(Mutex::new(String::new()));

    let observed_source = source_updates.clone();
    let observed_dependent = dependent_updates.clone();
    let source_for_events = source_id_slot.clone();
    let dependent_for_events = dependent_id_slot.clone();
    // Integration and dependent release are two different failures, so they get
    // two different counters keyed to two different task ids.
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
        .create("guard", "", "", "", "", None)
        .await
        .unwrap();
    let tasks = TaskRepository::new(db.clone(), events.clone());
    let task = tasks
        .create(&epic.id, "guard", "", "", "task", 0, "", Some("approved"))
        .await
        .unwrap();
    let dependent = tasks
        .create(&epic.id, "dependent", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    tasks.add_blocker(&dependent.id, &task.id).await.unwrap();
    *source_id_slot.lock().unwrap() = task.id.clone();
    *dependent_id_slot.lock().unwrap() = dependent.id.clone();

    // Every fixture below leaves `pr_url` null: direct routing must come from
    // the ledger, never from nullable task-PR data.
    assert!(task.pr_url.is_none());

    GuardFixture {
        db,
        tasks,
        task_id: task.id,
        source_updates,
        dependent_updates,
        events,
    }
}

impl GuardFixture {
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

    async fn guard_attempt_rows(&self) -> i64 {
        TaskAttemptRepository::new(self.db.clone())
            .list_for_task(&self.task_id)
            .await
            .map(|rows| i64::try_from(rows.len()).unwrap())
            .unwrap_or(0)
    }
}

/// Seed proposal ownership, an active attempt, and a delivery generation.
async fn seed_delivery(db: &Database, epic_id: &str, task_id: &str, state: Option<&str>) {
    djinn_db::test_support::seed_direct_delivery_liveness_fixture_for_test(
        db, epic_id, task_id, state,
    )
    .await;
}

async fn epic_of(db: &Database, task_id: &str) -> String {
    TaskRepository::new(db.clone(), EventBus::noop())
        .get(task_id)
        .await
        .unwrap()
        .unwrap()
        .epic_id
        .expect("fixture tasks are always created under an epic")
}

// ─── AC1 / AC2: Applying is consumed by the engine at this seam ─────────────

/// The guard does not merely classify `Applying` — it runs the landed engine
/// through the shared seam, and only then decides.
///
/// The ordering assertion is the point: `ResolveTaskActiveAttempt` must appear
/// before any append, adoption, or attempt row. A guard that adopted a PR first
/// and resolved ownership afterwards would still reach the same final state.
#[tokio::test]
async fn guard_seam_consumes_applying_through_the_engine_before_any_guard_effect() {
    let boundary = boundary_operations_scope().await;
    let fixture = guard_fixture().await;
    let epic_id = epic_of(&fixture.db, &fixture.task_id).await;
    seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;

    let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
    let engine = fixture.engine(remote.clone());
    let engine_runs = Arc::new(AtomicUsize::new(0));
    let runs = engine_runs.clone();

    let checkpoint = boundary.checkpoint();
    *fixture.source_updates.lock().unwrap() = 0;
    *fixture.dependent_updates.lock().unwrap() = 0;

    let decision = run_respawn_guard_with_reconciler(
        &fixture.db,
        &fixture.task_id,
        "worker",
        None,
        None,
        || {
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
        },
    )
    .await;

    let operations = boundary.operations_since(checkpoint);
    // Read every lock before the await below; a guard held across it would be a
    // real deadlock hazard, not just a lint.
    let remote_ref_pushes = remote.lock().unwrap().1;
    let source_task_updates = *fixture.source_updates.lock().unwrap();
    let dependent_releases = *fixture.dependent_updates.lock().unwrap();
    let guard_attempt_rows = fixture.guard_attempt_rows().await;
    let effects = GuardEffects {
        decision: format!("{decision:?}"),
        engine_runs: engine_runs.load(Ordering::SeqCst),
        remote_ref_pushes,
        task_pr_operations: operations
            .iter()
            .filter(|op| is_task_pr_operation(op))
            .count(),
        direct_appends: operations
            .iter()
            .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
            .count(),
        source_task_updates,
        dependent_releases,
        guard_attempt_rows,
    };

    assert_eq!(
        effects,
        GuardEffects {
            decision: "Defer(RespawnGuard)".to_owned(),
            engine_runs: 1,
            remote_ref_pushes: 1,
            task_pr_operations: 0,
            direct_appends: 1,
            source_task_updates: 1,
            dependent_releases: 1,
            guard_attempt_rows: 0,
        },
        "Applying must be consumed by the engine exactly once, integrate its source, \
         release its dependent, and reach no task-PR or guard-attempt effect"
    );

    // Canonical ownership resolution precedes every effect above.
    let resolve_at = operations
        .iter()
        .position(|op| matches!(op, BoundaryOperation::ResolveTaskActiveAttempt))
        .expect("the guard must resolve the canonical active attempt");
    let append_at = operations
        .iter()
        .position(|op| matches!(op, BoundaryOperation::DirectAppend))
        .expect("the engine must reach its append boundary");
    assert!(
        resolve_at < append_at,
        "ResolveTaskActiveAttempt must precede the direct append: {operations:?}"
    );
    assert!(
        operations
            .first()
            .is_some_and(|op| matches!(op, BoundaryOperation::CapabilityProbe)),
        "the epoch capability probe must come first: {operations:?}"
    );

    // Convergence to the exact candidate, read from the ledger.
    let generations =
        djinn_db::test_support::direct_delivery_generations_for_test(&fixture.db, &fixture.task_id)
            .await;
    assert_eq!(generations.len(), 1);
    assert_eq!(generations[0].state, "applied");
    assert_eq!(generations[0].candidate_sha, "fixture-candidate");
    let task = fixture.tasks.get(&fixture.task_id).await.unwrap().unwrap();
    assert_eq!(
        (task.status.as_str(), task.merge_commit_sha.as_deref()),
        ("closed", Some("fixture-candidate"))
    );
}

// ─── AC3: replay at the seam ───────────────────────────────────────────────

/// Running the production seam twice on exact `Applied` leaves everything
/// alone: no spawn, no reopen, no second append or push, no repeat integration
/// or dependent release.
#[tokio::test]
async fn guard_seam_replays_exact_applied_without_a_second_effect() {
    let boundary = boundary_operations_scope().await;
    let fixture = guard_fixture().await;
    let epic_id = epic_of(&fixture.db, &fixture.task_id).await;
    seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;

    let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
    let engine = fixture.engine(remote.clone());

    // Reach exact Applied + closed through the real engine and the real
    // TaskIntegrated transition, before the guard is ever entered.
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
        "engine did not integrate: {settled:?}"
    );

    let generations_before =
        djinn_db::test_support::direct_delivery_generations_for_test(&fixture.db, &fixture.task_id)
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
        "the pre-guard settle must have pushed once"
    );

    let engine_runs = Arc::new(AtomicUsize::new(0));

    for call in 1..=2 {
        *fixture.source_updates.lock().unwrap() = 0;
        *fixture.dependent_updates.lock().unwrap() = 0;
        let checkpoint = boundary.checkpoint();
        let runs = engine_runs.clone();

        let decision = run_respawn_guard_with_reconciler(
            &fixture.db,
            &fixture.task_id,
            "worker",
            None,
            None,
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
        let guard_attempt_rows = fixture.guard_attempt_rows().await;
        let effects = GuardEffects {
            decision: format!("{decision:?}"),
            engine_runs: engine_runs.load(Ordering::SeqCst),
            remote_ref_pushes,
            task_pr_operations: operations
                .iter()
                .filter(|op| is_task_pr_operation(op))
                .count(),
            direct_appends: operations
                .iter()
                .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
                .count(),
            source_task_updates,
            dependent_releases,
            guard_attempt_rows,
        };
        assert_eq!(
            effects,
            GuardEffects {
                decision: "Defer(RespawnGuard)".to_owned(),
                engine_runs: 0,
                remote_ref_pushes: 0,
                task_pr_operations: 0,
                direct_appends: 0,
                source_task_updates: 0,
                dependent_releases: 0,
                guard_attempt_rows: 0,
            },
            "call {call}: a settled generation must produce no guard, spawn, or delivery effect"
        );
        assert_eq!(
            fixture
                .tasks
                .get(&fixture.task_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "closed",
            "call {call}: closed status must survive the guard"
        );
        assert_eq!(
            djinn_db::test_support::direct_delivery_generations_for_test(
                &fixture.db,
                &fixture.task_id
            )
            .await,
            generations_before,
            "call {call}: the immutable generation must be unchanged"
        );
        assert_eq!(
            djinn_db::test_support::direct_delivery_candidate_cardinality_for_test(
                &fixture.db,
                &fixture.task_id
            )
            .await,
            cardinality_before,
            "call {call}: candidate cardinality must not grow"
        );
    }
}

/// A `Conflict` generation defers without touching anything.
#[tokio::test]
async fn guard_seam_defers_conflict_without_mutation() {
    let fixture = guard_fixture().await;
    let epic_id = epic_of(&fixture.db, &fixture.task_id).await;
    seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("conflict")).await;

    let generations_before =
        djinn_db::test_support::direct_delivery_generations_for_test(&fixture.db, &fixture.task_id)
            .await;
    let status_before = fixture
        .tasks
        .get(&fixture.task_id)
        .await
        .unwrap()
        .unwrap()
        .status;
    *fixture.source_updates.lock().unwrap() = 0;

    let decision = run_respawn_guard_with_reconciler(
        &fixture.db,
        &fixture.task_id,
        "worker",
        None,
        None,
        || async { panic!("Conflict must not enter the delivery engine") },
    )
    .await;

    assert!(matches!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    ));
    assert_eq!(*fixture.source_updates.lock().unwrap(), 0);
    assert_eq!(fixture.guard_attempt_rows().await, 0);
    assert_eq!(
        djinn_db::test_support::direct_delivery_generations_for_test(&fixture.db, &fixture.task_id)
            .await,
        generations_before
    );
    assert_eq!(
        fixture
            .tasks
            .get(&fixture.task_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        status_before
    );
}

/// Applied + closed with **no** `pr_url` still defers. A guard that inferred
/// "no PR, so spawn a worker" would reopen a completed direct delivery.
#[tokio::test]
async fn guard_seam_defers_applied_closed_without_pr_url() {
    let fixture = guard_fixture().await;
    let epic_id = epic_of(&fixture.db, &fixture.task_id).await;
    seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applied")).await;
    fixture
        .tasks
        .set_status(&fixture.task_id, "closed")
        .await
        .unwrap();

    let task = fixture.tasks.get(&fixture.task_id).await.unwrap().unwrap();
    assert!(task.pr_url.is_none(), "the deferral must not need a PR URL");

    let decision = run_respawn_guard_with_reconciler(
        &fixture.db,
        &fixture.task_id,
        "worker",
        None,
        None,
        || async { panic!("Applied must not enter the delivery engine") },
    )
    .await;

    assert!(
        matches!(
            decision,
            RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
        ),
        "Applied/closed without pr_url must defer, got {decision:?}"
    );
    assert_eq!(fixture.guard_attempt_rows().await, 0);
}

// ─── AC4: fail-closed and retained-legacy, local to this seam ──────────────

#[derive(Clone, Copy, Debug)]
enum GuardRoutingCase {
    UnresolvedOwnership,
    MissingSchema,
    MissingEpoch,
    UnknownEpoch,
    UnknownDeliveryState,
    SupportedDisabled,
    SupportedActiveExplicitLegacy,
}

/// Fail-closed and retained-legacy routing, proven at the guard seam itself
/// rather than inferred from ready-dispatch coverage.
///
/// The retained-legacy cases carry a real open `pr_url` so the assertion is
/// positive: they must still reach the legacy adoption behavior, not merely
/// "not fail". The fail-closed cases carry the same `pr_url`, which makes the
/// test sharp — a guard that fell through to step 1 would adopt that PR, and
/// adoption is a mutation.
#[tokio::test]
async fn guard_seam_fails_closed_and_preserves_legacy_adoption_by_persisted_state() {
    const PR: &str = "https://example.test/pr/guard";

    for case in [
        GuardRoutingCase::UnresolvedOwnership,
        GuardRoutingCase::MissingSchema,
        GuardRoutingCase::MissingEpoch,
        GuardRoutingCase::UnknownEpoch,
        GuardRoutingCase::UnknownDeliveryState,
        GuardRoutingCase::SupportedDisabled,
        GuardRoutingCase::SupportedActiveExplicitLegacy,
    ] {
        let fixture = guard_fixture().await;
        let epic_id = epic_of(&fixture.db, &fixture.task_id).await;

        match case {
            GuardRoutingCase::UnresolvedOwnership => {
                djinn_db::test_support::activate_direct_delivery_epoch_for_test(&fixture.db).await;
                djinn_db::test_support::seed_direct_delivery_proposal_for_test(
                    &fixture.db,
                    &fixture.task_id,
                    &fixture.task_id[..8],
                )
                .await;
            }
            GuardRoutingCase::MissingSchema => {
                seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;
                djinn_db::test_support::drop_table_cascade_for_test(&fixture.db, "task_deliveries")
                    .await;
            }
            GuardRoutingCase::MissingEpoch => {
                seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;
                djinn_db::test_support::remove_direct_delivery_epoch_for_test(&fixture.db).await;
            }
            GuardRoutingCase::UnknownEpoch => {
                seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;
                djinn_db::test_support::seed_unknown_direct_delivery_epoch_for_test(&fixture.db)
                    .await;
            }
            GuardRoutingCase::UnknownDeliveryState => {
                seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;
                djinn_db::test_support::seed_unknown_task_delivery_state_for_test(
                    &fixture.db,
                    &fixture.task_id,
                    "quiesced",
                )
                .await;
            }
            GuardRoutingCase::SupportedDisabled => {
                seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;
                djinn_db::test_support::disable_direct_delivery_epoch_for_test(&fixture.db).await;
            }
            GuardRoutingCase::SupportedActiveExplicitLegacy => {
                seed_delivery(&fixture.db, &epic_id, &fixture.task_id, Some("applying")).await;
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

        let decision = run_respawn_guard_with_reconciler(
            &fixture.db,
            &fixture.task_id,
            "worker",
            Some(PR),
            None,
            || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                panic!("{case:?} must never reach the delivery engine")
            },
        )
        .await;

        match case {
            GuardRoutingCase::UnresolvedOwnership
            | GuardRoutingCase::MissingSchema
            | GuardRoutingCase::MissingEpoch
            | GuardRoutingCase::UnknownEpoch
            | GuardRoutingCase::UnknownDeliveryState => {
                assert!(
                    matches!(
                        decision,
                        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
                    ),
                    "{case:?}: must fail closed, got {decision:?}"
                );
                assert_eq!(
                    engine_runs.load(Ordering::SeqCst),
                    0,
                    "{case:?}: a fail-closed contract must not run the engine"
                );
                assert_eq!(
                    *fixture.source_updates.lock().unwrap(),
                    0,
                    "{case:?}: failing closed must not mutate the task"
                );
                assert_eq!(
                    fixture.guard_attempt_rows().await,
                    0,
                    "{case:?}: failing closed must not record a guard attempt"
                );
            }
            GuardRoutingCase::SupportedDisabled
            | GuardRoutingCase::SupportedActiveExplicitLegacy => {
                assert!(
                    matches!(
                        &decision,
                        RespawnGuardDecision::Adopted { pr_url } if pr_url == PR
                    ),
                    "{case:?}: retained legacy must preserve open-PR adoption, got {decision:?}"
                );
                assert_eq!(
                    engine_runs.load(Ordering::SeqCst),
                    0,
                    "{case:?}: retained legacy must not run the direct engine"
                );
            }
        }
    }
}
