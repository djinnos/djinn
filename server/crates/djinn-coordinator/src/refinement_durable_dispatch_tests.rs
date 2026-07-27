use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    models::TaskRefinementCorrelation,
    refinement_liveness::{
        DbTimestamp, RefinementIntentState, RefinementPhase, RefinementRunState,
        RefinementStopReason,
    },
};
use djinn_db::{
    AdmitRefinementRunRequest, ClaimRefinementIntentRequest, LoadRefinementRunSnapshotRequest,
    ProposalRepository, RefinementAdmissionOutcome, RefinementAdmissionSource,
    ReleaseRefinementIntentClaimRequest, TaskRepository, TerminalRefinementRunRequest,
};
use djinn_provider::repos::CredentialRepository;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::refinement_cap_tests;
use crate::types::{REFINEMENT_INTENT_CLAIM_LEASE, STUCK_INTERVAL};

const DURABLE_PROVIDER: &str = "durableobserver";
const DURABLE_MODEL: &str = "durableobserver/configured-model";

fn spawn_model_observing_pool(
    db: &djinn_db::Database,
    model_id: &str,
) -> (
    djinn_slot::SlotPoolHandle,
    tokio::sync::mpsc::UnboundedReceiver<(String, String)>,
) {
    let cancel = CancellationToken::new();
    let (observed_tx, observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let model_id = model_id.to_owned();
    let pool = djinn_slot::SlotPoolHandle::spawn_with_factory(
        crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
        cancel,
        djinn_slot::SlotPoolConfig {
            models: vec![djinn_slot::ModelSlotConfig {
                model_id: model_id.clone(),
                max_slots: 1,
                roles: ["advocate", "adversary", "judge", "worker"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
        Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
            let observed_tx = observed_tx.clone();
            let runner: djinn_slot::TestLifecycleRunner =
                Arc::new(move |task_id, _, model_id, _, _, _, _| {
                    let observed_tx = observed_tx.clone();
                    Box::pin(async move {
                        observed_tx
                            .send((task_id, model_id))
                            .expect("record durable dispatch model");
                        Ok(())
                    })
                });
            djinn_slot::SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    (pool, observed_rx)
}

async fn wait_until_pool_releases(pool: &djinn_slot::SlotPoolHandle, task_id: &str) {
    for _ in 0..50 {
        if !pool
            .has_session(task_id)
            .await
            .expect("query observing pool session")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("observing pool did not release {task_id}");
}

async fn admit_run(repo: &ProposalRepository, proposal_id: &str, key: &str) -> (String, i32) {
    match repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: proposal_id.into(),
            idempotency_key: key.into(),
            source: RefinementAdmissionSource::Demand {
                demand_id: format!("demand-{key}"),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit durable refinement run")
    {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    }
}

async fn only_intent(repo: &ProposalRepository, run_id: &str, generation: i32) -> String {
    let intents = repo
        .load_dispatchable_refinement_intents(run_id, generation)
        .await
        .expect("read exact-run dispatchable intent");
    assert_eq!(intents.len(), 1, "fixture has exactly one initial intent");
    intents[0].intent_id.clone()
}

async fn heartbeat(repo: &ProposalRepository, run_id: &str) -> DbTimestamp {
    repo.load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
        run_id: run_id.into(),
        heartbeat_grace_millis: 60_000,
    })
    .await
    .expect("read exact-run snapshot")
    .expect("run exists")
    .snapshot
    .heartbeat
    .expect("run has heartbeat")
    .heartbeat_at
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_driver_replays_claim_before_task_and_cache_loss_once_without_poll_heartbeat() {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    CredentialRepository::new(db.clone(), EventBus::noop())
        .set_with_owner(
            DURABLE_PROVIDER,
            "DURABLEOBSERVER_API_KEY",
            "durable-observer-secret",
            Some(&fixture.user_id),
        )
        .await
        .expect("seed durable dispatch credential");
    let (pool, mut observed_dispatches) = spawn_model_observing_pool(&db, DURABLE_MODEL);
    let mut actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, pool);
    actor.test_use_live_credential_resolution = true;
    actor.catalog.add_custom_provider(
        refinement_cap_tests::test_provider(DURABLE_PROVIDER),
        vec![refinement_cap_tests::test_model(
            "configured-model",
            DURABLE_PROVIDER,
        )],
    );
    actor
        .model_priorities
        .insert("planner".to_owned(), vec![DURABLE_MODEL.to_owned()]);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation) = admit_run(&repo, &fixture.proposal_id, "durable-claim-replay").await;
    let intent_id = only_intent(&repo, &run_id, generation).await;
    let projection =
        super::super::refinement::RefinementLoopState::new(fixture.proposal_id.clone(), 0)
            .with_run_identity(run_id.clone(), generation)
            .with_attributed_user(Some(fixture.user_id.clone()));
    actor.active_refinements.insert(run_id.clone(), projection);

    // Simulate a process dying after the lease CAS and before task creation. A
    // pure drive cannot steal the unexpired foreign lease or mutate heartbeat.
    repo.claim_refinement_intent(ClaimRefinementIntentRequest {
        run_id: run_id.clone(),
        intent_id: intent_id.clone(),
        generation,
        owner: "crashed-materializer".into(),
        lease_millis: 60_000,
    })
    .await
    .expect("claim intent")
    .expect("lease acquired");
    let heartbeat_before_poll = heartbeat(&repo, &run_id).await;
    actor.drive_active_refinements().await;
    assert!(
        TaskRepository::new(db.clone(), EventBus::noop())
            .find_by_refinement_intent_id(&intent_id)
            .await
            .expect("read task")
            .is_none(),
        "foreign unexpired claim is fenced at the dispatch layer"
    );
    assert_eq!(heartbeat_before_poll, heartbeat(&repo, &run_id).await);

    repo.release_refinement_intent_claim(ReleaseRefinementIntentClaimRequest {
        run_id: run_id.clone(),
        intent_id: intent_id.clone(),
        generation,
        owner: "crashed-materializer".into(),
    })
    .await
    .expect("release simulated crash lease");
    actor.drive_active_refinements().await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let first_task = task_repo
        .find_by_refinement_intent_id(&intent_id)
        .await
        .expect("read materialized task")
        .expect("driver materializes task after replay");
    assert_eq!(
        first_task.refinement_run_id.as_deref(),
        Some(run_id.as_str())
    );
    assert_eq!(
        first_task.refinement_generation,
        Some(i64::from(generation))
    );
    assert_eq!(
        first_task.refinement_intent_id.as_deref(),
        Some(intent_id.as_str())
    );
    let refinement_tasks = task_repo
        .list_by_project(&fixture.project_id)
        .await
        .expect("list refinement role tasks")
        .into_iter()
        .filter(|task| task.issue_type == "refinement")
        .collect::<Vec<_>>();
    assert_eq!(
        refinement_tasks.len(),
        1,
        "populated run cache must not invoke the legacy map-driven role spawner"
    );
    assert_eq!(refinement_tasks[0].id, first_task.id);
    let (initial_task_id, initial_model_id) =
        tokio::time::timeout(Duration::from_secs(1), observed_dispatches.recv())
            .await
            .expect("initial durable enqueue reaches observing runner")
            .expect("initial durable enqueue observation");
    assert_eq!(initial_task_id, first_task.id);
    assert_eq!(initial_model_id, DURABLE_MODEL);
    assert_ne!(initial_model_id, refinement_cap_tests::TEST_MODEL);
    assert_eq!(
        repo.load_dispatchable_refinement_intents(&run_id, generation)
            .await
            .expect("read materialized intent")[0]
            .state,
        RefinementIntentState::Materialized
    );
    let session = actor
        .refinement_sessions
        .get(&run_id)
        .expect("successful durable enqueue creates the run-keyed outcome projection");
    assert_eq!(session.run_id, run_id);
    assert_eq!(session.generation, generation);
    assert_eq!(session.task_id, first_task.id);
    assert_eq!(
        session.phase,
        super::super::refinement::RefinementPhase::AdversaryAttack
    );
    assert_eq!(session.model_id, DURABLE_MODEL);
    assert!(session.session_started_at.is_none());

    // Cache loss is not authority: the next durable poll finds the same task,
    // and read-only polling/enqueue retry does not keep the run alive.
    actor.active_refinements.clear();
    actor.refinement_sessions.clear();
    let heartbeat_after_materialization = heartbeat(&repo, &run_id).await;
    wait_until_pool_releases(&actor.pool, &first_task.id).await;
    actor.drive_active_refinements().await;
    assert_eq!(
        task_repo
            .find_by_refinement_intent_id(&intent_id)
            .await
            .expect("read replay task")
            .expect("replay task exists")
            .id,
        first_task.id
    );
    assert_eq!(
        heartbeat_after_materialization,
        heartbeat(&repo, &run_id).await
    );
    let (retry_task_id, retry_model_id) =
        tokio::time::timeout(Duration::from_secs(1), observed_dispatches.recv())
            .await
            .expect("materialized retry reaches observing runner")
            .expect("materialized retry observation");
    assert_eq!(retry_task_id, first_task.id);
    assert_eq!(retry_model_id, DURABLE_MODEL);
    assert_ne!(retry_model_id, refinement_cap_tests::TEST_MODEL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_driver_accepts_task_before_ack_and_fences_predecessor_evidence() {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let mut actor = refinement_cap_tests::build_refinement_actor(
        &db,
        &events_tx,
        refinement_cap_tests::spawn_test_pool(&db, 1),
    );
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (old_run, old_generation) = admit_run(&repo, &fixture.proposal_id, "durable-old").await;
    let old_intent = only_intent(&repo, &old_run, old_generation).await;

    // A correlated task written before the acknowledgement is accepted on
    // replay, rather than producing a replacement role task.
    repo.claim_refinement_intent(ClaimRefinementIntentRequest {
        run_id: old_run.clone(),
        intent_id: old_intent.clone(),
        generation: old_generation,
        owner: "crashed-after-task".into(),
        lease_millis: 60_000,
    })
    .await
    .expect("claim old intent")
    .expect("old lease");
    let correlation = TaskRefinementCorrelation::new(
        old_run.clone(),
        old_intent.clone(),
        i64::from(old_generation),
        1,
        RefinementPhase::AdversaryAttack,
        djinn_core::refinement_liveness::RefinementRole::Adversary,
    )
    .expect("valid correlation");
    let task_id = actor
        .create_refinement_task_with_context_and_correlation(
            &fixture.proposal_id,
            "adversary",
            1,
            0,
            "crash fixture",
            None,
            Some(&fixture.user_id),
            Some(&correlation),
        )
        .await
        .expect("durably insert correlated task before ack");
    repo.release_refinement_intent_claim(ReleaseRefinementIntentClaimRequest {
        run_id: old_run.clone(),
        intent_id: old_intent.clone(),
        generation: old_generation,
        owner: "crashed-after-task".into(),
    })
    .await
    .expect("release old lease");
    actor.drive_active_refinements().await;
    assert_eq!(
        TaskRepository::new(db.clone(), EventBus::noop())
            .find_by_refinement_intent_id(&old_intent)
            .await
            .expect("read old task")
            .expect("old task remains")
            .id,
        task_id,
        "task-before-ack replay must retain the original correlated task"
    );

    repo.terminal_refinement_run(TerminalRefinementRunRequest {
        run_id: old_run.clone(),
        generation: old_generation,
        reason: RefinementStopReason::Interrupted {
            detail: Some("predecessor fixture".into()),
        },
    })
    .await
    .expect("terminalize predecessor");
    let (current_run, current_generation) =
        admit_run(&repo, &fixture.proposal_id, "durable-current").await;
    let current_intent = only_intent(&repo, &current_run, current_generation).await;
    actor.drive_active_refinements().await;
    let current_task = TaskRepository::new(db.clone(), EventBus::noop())
        .find_by_refinement_intent_id(&current_intent)
        .await
        .expect("read current task")
        .expect("current run materialized");
    assert_ne!(current_task.id, task_id);
    assert_eq!(
        current_task.refinement_run_id.as_deref(),
        Some(current_run.as_str())
    );
    assert_eq!(
        TaskRepository::new(db.clone(), EventBus::noop())
            .find_by_refinement_intent_id(&old_intent)
            .await
            .expect("read predecessor task")
            .expect("predecessor task remains")
            .id,
        task_id,
        "predecessor task evidence cannot be reused by current generation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn materialized_enqueue_failure_is_retried_with_the_same_task() {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let cancel = CancellationToken::new();
    let dead_pool = djinn_slot::SlotPoolHandle::spawn(
        crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
        cancel.clone(),
        djinn_slot::SlotPoolConfig {
            models: vec![djinn_slot::ModelSlotConfig {
                model_id: refinement_cap_tests::TEST_MODEL.into(),
                max_slots: 1,
                roles: ["advocate", "adversary", "judge", "worker"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
    );
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, dead_pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation) =
        admit_run(&repo, &fixture.proposal_id, "durable-enqueue-retry").await;
    let intent_id = only_intent(&repo, &run_id, generation).await;
    let lifecycle_before = repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle before enqueue failure")
        .into_iter()
        .filter(|revision| revision.event_kind == "refinement_stop")
        .count();
    let projection =
        super::super::refinement::RefinementLoopState::new(fixture.proposal_id.clone(), 0)
            .with_run_identity(run_id.clone(), generation)
            .with_attributed_user(Some(fixture.user_id.clone()));
    actor.active_refinements.insert(run_id.clone(), projection);
    let projection_before = format!("{:#?}", actor.active_refinements[&run_id]);

    actor.drive_active_refinements().await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task_id = task_repo
        .find_by_refinement_intent_id(&intent_id)
        .await
        .expect("read failed enqueue task")
        .expect("task is durable despite enqueue failure")
        .id;
    assert_eq!(
        repo.load_dispatchable_refinement_intents(&run_id, generation)
            .await
            .expect("read retry intent")[0]
            .state,
        RefinementIntentState::Materialized
    );
    let failed_enqueue_snapshot = repo
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: run_id.clone(),
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("read exact run after enqueue failure")
        .expect("exact run remains");
    assert_eq!(
        failed_enqueue_snapshot.snapshot.run.state,
        RefinementRunState::Active
    );
    assert_eq!(failed_enqueue_snapshot.snapshot.run.terminal_reason, None);
    assert_eq!(
        format!("{:#?}", actor.active_refinements[&run_id]),
        projection_before,
        "enqueue failure must not destructively mutate the run projection"
    );
    assert_eq!(
        repo.revisions(&fixture.proposal_id)
            .await
            .expect("read lifecycle after enqueue failure")
            .into_iter()
            .filter(|revision| revision.event_kind == "refinement_stop")
            .count(),
        lifecycle_before,
        "pool failure must not append a proposal-scoped lifecycle stop"
    );
    assert!(
        actor.refinement_sessions.is_empty(),
        "failed enqueue must not manufacture an outcome projection"
    );

    // Model a coordinator crash after the materialization acknowledgement but
    // before a successful pool enqueue. Neither the projection nor its
    // in-memory retry bookkeeping is authoritative: a fresh actor must rebuild
    // from the exact durable run and enqueue the already-created task.
    drop(actor);
    let (live_pool, mut restarted_dispatches) =
        spawn_model_observing_pool(&db, refinement_cap_tests::TEST_MODEL);
    let mut restarted = refinement_cap_tests::build_refinement_actor(&db, &events_tx, live_pool);
    restarted.recover_interrupted_refinements().await;
    let rebuilt = restarted
        .active_refinements
        .get(&run_id)
        .expect("restart rebuilds the exact current run projection");
    assert_eq!(rebuilt.run_id, run_id);
    assert_eq!(rebuilt.generation, generation);
    assert_eq!(
        rebuilt.phase,
        super::super::refinement::RefinementPhase::AdversaryAttack
    );
    assert_eq!(rebuilt.current_round, 1);

    restarted.drive_active_refinements().await;
    let (restarted_task_id, restarted_model_id) =
        tokio::time::timeout(Duration::from_secs(1), restarted_dispatches.recv())
            .await
            .expect("the materialized task is eventually enqueued after restart")
            .expect("restarted durable enqueue observation");
    assert_eq!(restarted_task_id, task_id);
    assert_eq!(restarted_model_id, refinement_cap_tests::TEST_MODEL);
    let retried_session = restarted
        .refinement_sessions
        .get(&run_id)
        .expect("successful materialized retry repairs the run-keyed projection");
    assert_eq!(retried_session.run_id, run_id);
    assert_eq!(retried_session.generation, generation);
    assert_eq!(retried_session.task_id, task_id);
    assert_eq!(
        retried_session.phase,
        super::super::refinement::RefinementPhase::AdversaryAttack
    );
    assert_eq!(retried_session.model_id, refinement_cap_tests::TEST_MODEL);
    assert!(retried_session.session_started_at.is_none());
    assert_eq!(
        task_repo
            .find_by_refinement_intent_id(&intent_id)
            .await
            .expect("read retried task")
            .expect("same task remains retryable")
            .id,
        task_id
    );
    assert_eq!(
        task_repo
            .find_by_refinement_intent_id(&intent_id)
            .await
            .expect("read retry task again")
            .expect("same task remains")
            .id,
        task_id
    );
}

/// Regression (proposal bzpt, 2026-07-26): the durable intent ledger is the
/// ONLY dispatch path for a run with a `run_id` — `drive_one_refinement`
/// returns early before the legacy `dispatch_next_refinement_phase` for any
/// durable run. That ledger path used to inject the fixed literal
/// `"Durable refinement intent dispatch."` as the readiness context and pass
/// `None` for reviewer feedback, so every live tribunal role was dispatched
/// blind:
///
/// * the Advocate never learned which DoR checks were failing, and
/// * the Judge — whose prompt makes any injected `Current DoR status` other
///   than the exact clean message a mandatory blocking reject — could never
///   approve, regardless of spec quality.
///
/// On bzpt this produced advocate task pw4v whose entire description was
/// "Current DoR status: Durable refinement intent dispatch." and a judge
/// verdict that rejected a spec it had otherwise found complete.
///
/// This drives the real durable driver and asserts the materialized task
/// carries the head-derived DoR failures, the lint summary, and the current
/// reviewer feedback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_dispatch_injects_real_dor_findings_not_a_placeholder() {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = refinement_cap_tests::spawn_test_pool(&db, 4);
    let mut actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, pool);

    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    // Human demand-round feedback recorded against the current revision. The
    // durable path used to drop this on the floor.
    let reviewer_feedback = "REVIEWER-FEEDBACK-durable-path-bzpt";
    repo.record_refinement_lifecycle(
        &fixture.proposal_id,
        "refinement_start",
        Some(&serde_json::json!({
            "source": "human_demand_round",
            "reviewer_feedback": reviewer_feedback,
            "reason": reviewer_feedback,
        })),
    )
    .await
    .expect("record demand-round reviewer feedback");

    let (run_id, generation) =
        admit_run(&repo, &fixture.proposal_id, "durable-dor-injection").await;
    let intent_id = only_intent(&repo, &run_id, generation).await;
    actor.active_refinements.insert(
        run_id.clone(),
        super::super::refinement::RefinementLoopState::new(fixture.proposal_id.clone(), 0)
            .with_run_identity(run_id.clone(), generation)
            .with_attributed_user(Some(fixture.user_id.clone())),
    );

    // Independently establish what the shared evaluator says about this head,
    // so the assertions below are pinned to real findings rather than to any
    // string the dispatcher happens to emit.
    let readiness = actor
        .evaluate_proposal_readiness(&fixture.proposal_id)
        .await
        .expect("fixture readiness resolves");
    assert!(
        !readiness.ready,
        "fixture head (one-line body, empty ACs) must fail DoR for this test to be non-vacuous"
    );
    let expected_failures = readiness
        .to_error_string()
        .expect("failing head must render failure text");

    actor.drive_active_refinements().await;

    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .find_by_refinement_intent_id(&intent_id)
        .await
        .expect("read materialized task")
        .expect("durable driver materialized a task");

    assert!(
        !task
            .description
            .contains("Durable refinement intent dispatch"),
        "durable dispatch must not inject a placeholder readiness context:\n{}",
        task.description
    );
    assert!(
        task.description.contains(&expected_failures),
        "durable dispatch must inject the head-derived DoR failures ({expected_failures}):\n{}",
        task.description
    );
    assert!(
        task.description
            .contains("Latest SpecLintResultV1 summary (errors and warnings):"),
        "durable dispatch must inject the shared lint summary:\n{}",
        task.description
    );
    assert!(
        !task
            .description
            .contains("Proposal currently meets all DoR checks."),
        "a failing head must never render the clean DoR message the Judge treats as pass:\n{}",
        task.description
    );
    assert!(
        task.description.contains(reviewer_feedback),
        "durable dispatch must inject current-revision reviewer feedback:\n{}",
        task.description
    );
}

// ── Claim lease vs durable poll interval ─────────────────────────────────────

/// Simulate `elapsed` of wall-clock time passing for one exact run.
async fn elapse_durable_wall_clock(db: &djinn_db::Database, run_id: &str, elapsed: Duration) {
    djinn_db::test_support::elapse_refinement_run_wall_clock_for_test(
        db,
        run_id,
        elapsed.as_secs() as i64,
    )
    .await;
}

async fn exact_liveness(
    repo: &ProposalRepository,
    run_id: &str,
) -> djinn_core::refinement_liveness::RefinementLivenessResult {
    repo.load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
        run_id: run_id.to_owned(),
        heartbeat_grace_millis: 60_000,
    })
    .await
    .expect("read exact-run snapshot")
    .expect("run exists")
    .liveness
}

/// A live refinement run must never present as evidence-less between two
/// durable polls.
///
/// While an intent is `claimed` but not yet `materialized` — attribution not
/// resolved yet, admission denied, cap-3 contention, a slow coordinator tick —
/// the unexpired claim lease is the ONLY liveness evidence the run has, and
/// `drive_active_refinements` is the only thing that can renew it.
///
/// Two independent ways that relationship breaks, both covered here:
///
///  1. The lease is shorter than the interval between the polls that renew it,
///     so it lapses in every cycle rather than occasionally.
///  2. The CAS refuses to renew a lease that has not already lapsed, making
///     expiry a *precondition* of renewal. That window exists for any lease
///     length, so it cannot be closed by picking a bigger number.
///
/// Either way the live run holds an expired claim, matches no evidence class in
/// `evaluate_refinement_liveness`, and `reap_and_admit` terminalizes it as
/// `reaped_phantom` while minting generation N+1 — the mechanism that drove
/// `8ixk` to generation 5.
///
/// The loop runs more polls than the lease alone could ever cover
/// (`REFINEMENT_INTENT_CLAIM_LEASE / STUCK_INTERVAL + 2`) and observes at the
/// worst moment of every cycle: immediately before the renewing poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_run_never_holds_an_expired_claim_between_durable_polls() {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let (pool, _observed_dispatches) = spawn_model_observing_pool(&db, DURABLE_MODEL);
    let mut actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation) = admit_run(&repo, &fixture.proposal_id, "claim-lease-renewal").await;
    let intent_id = only_intent(&repo, &run_id, generation).await;

    // Hold the intent in `claimed` across polls exactly the way production does
    // for a run that is slow to materialize: the durable pass takes the claim
    // and then bails at the attribution gate. No task, no session, no
    // materialized intent — the claim lease is the only evidence that remains.
    repo.start_refinement_with_owner(&fixture.proposal_id, None)
        .await
        .expect("suspend durable refinement attribution");

    actor.drive_active_refinements().await;
    let claimed = repo
        .load_dispatchable_refinement_intents(&run_id, generation)
        .await
        .expect("read exact-run intents");
    assert_eq!(
        claimed[0].state,
        RefinementIntentState::Claimed,
        "the durable pass must claim the intent and then stall at the attribution gate"
    );
    assert!(
        TaskRepository::new(db.clone(), EventBus::noop())
            .find_by_refinement_intent_id(&intent_id)
            .await
            .expect("read materialized task")
            .is_none(),
        "an unattributed intent must not materialize a task; the claim is the only evidence"
    );

    let polls = REFINEMENT_INTENT_CLAIM_LEASE.as_secs() / STUCK_INTERVAL.as_secs() + 2;
    for poll in 1..=polls {
        elapse_durable_wall_clock(&db, &run_id, STUCK_INTERVAL).await;
        let liveness = exact_liveness(&repo, &run_id).await;
        assert!(
            matches!(
                liveness,
                djinn_core::refinement_liveness::RefinementLivenessResult::Live { .. }
            ),
            "poll {poll}/{polls}: a live run whose claim lapsed between polls matched no \
             liveness evidence class {}s after its last renewal: {liveness:?}",
            STUCK_INTERVAL.as_secs() * poll
        );
        actor.drive_active_refinements().await;
    }

    // The harm, not the label. `reap_and_admit` is what the admission path calls
    // on every start/demand/revision; a `Stale` verdict at this observation is
    // what terminalizes a live run and mints its successor generation.
    elapse_durable_wall_clock(&db, &run_id, STUCK_INTERVAL).await;
    let refused = repo
        .reap_and_admit(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id.clone(),
            idempotency_key: "reaper-observes-live-run".into(),
            source: RefinementAdmissionSource::Demand {
                demand_id: "reaper-observes-live-run".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await;
    assert!(
        matches!(
            refused,
            Err(djinn_db::RefinementAdmissionError::AlreadyActive { .. })
        ),
        "the admission reaper must refuse a live run, not terminalize it: {refused:?}"
    );
    let live_audit = djinn_db::test_support::refinement_run_audit_for_test(&db, &run_id).await;
    assert_eq!(live_audit.state, "running", "a live run must stay running");
    assert_eq!(
        live_audit.typed_reap_count, 0,
        "a live run must never be written as reaped_phantom"
    );

    // The reaper itself must NOT be weakened: an owner that actually stopped
    // renewing — a dead coordinator — still lapses, still reads stale, and is
    // still reaped.
    elapse_durable_wall_clock(&db, &run_id, REFINEMENT_INTENT_CLAIM_LEASE + STUCK_INTERVAL).await;
    let abandoned = exact_liveness(&repo, &run_id).await;
    assert!(
        matches!(
            abandoned,
            djinn_core::refinement_liveness::RefinementLivenessResult::Stale { .. }
        ),
        "an abandoned claim past its whole lease must still read stale: {abandoned:?}"
    );
    let reaped = repo
        .reap_and_admit(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id.clone(),
            idempotency_key: "reaper-observes-abandoned-run".into(),
            source: RefinementAdmissionSource::Demand {
                demand_id: "reaper-observes-abandoned-run".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("abandoned run is reapable");
    assert!(
        matches!(
            reaped,
            RefinementAdmissionOutcome::Admitted { generation: 2, .. }
        ),
        "a genuinely abandoned run must still be reaped into a successor generation: {reaped:?}"
    );
    let reaped_audit = djinn_db::test_support::refinement_run_audit_for_test(&db, &run_id).await;
    assert_eq!(
        reaped_audit.stop_tag.as_deref(),
        Some("reaped_phantom"),
        "the reaper must still write reaped_phantom for a genuinely stale run"
    );
}
