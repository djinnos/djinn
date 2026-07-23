use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    models::TaskRefinementCorrelation,
    refinement_liveness::{DbTimestamp, RefinementIntentState, RefinementPhase, RefinementStopReason},
};
use djinn_db::{
    AdmitRefinementRunRequest, ClaimRefinementIntentRequest, LoadRefinementRunSnapshotRequest,
    ProposalRepository,
    RefinementAdmissionOutcome, RefinementAdmissionSource, ReleaseRefinementIntentClaimRequest,
    TaskRepository, TerminalRefinementRunRequest,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::refinement_cap_tests;

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
    let mut actor = refinement_cap_tests::build_refinement_actor(
        &db,
        &events_tx,
        refinement_cap_tests::spawn_test_pool(&db, 1),
    );
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation) = admit_run(&repo, &fixture.proposal_id, "durable-claim-replay").await;
    let intent_id = only_intent(&repo, &run_id, generation).await;

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
    assert_eq!(first_task.refinement_run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(first_task.refinement_generation, Some(i64::from(generation)));
    assert_eq!(first_task.refinement_intent_id.as_deref(), Some(intent_id.as_str()));
    assert_eq!(
        repo.load_dispatchable_refinement_intents(&run_id, generation)
            .await
            .expect("read materialized intent")[0]
            .state,
        RefinementIntentState::Materialized
    );

    // Cache loss is not authority: the next durable poll finds the same task,
    // and read-only polling/enqueue retry does not keep the run alive.
    actor.active_refinements.clear();
    actor.refinement_sessions.clear();
    let heartbeat_after_materialization = heartbeat(&repo, &run_id).await;
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
    assert_eq!(heartbeat_after_materialization, heartbeat(&repo, &run_id).await);
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
        run_id: old_run.clone(), intent_id: old_intent.clone(), generation: old_generation,
        owner: "crashed-after-task".into(), lease_millis: 60_000,
    }).await.expect("claim old intent").expect("old lease");
    let correlation = TaskRefinementCorrelation::new(
        old_run.clone(), old_intent.clone(), i64::from(old_generation), 1,
        RefinementPhase::AdversaryAttack,
        djinn_core::refinement_liveness::RefinementRole::Adversary,
    ).expect("valid correlation");
    let task_id = actor.create_refinement_task_with_context_and_correlation(
        &fixture.proposal_id, "adversary", 1, 0, "crash fixture", None,
        Some(&fixture.user_id), Some(&correlation),
    ).await.expect("durably insert correlated task before ack");
    repo.release_refinement_intent_claim(ReleaseRefinementIntentClaimRequest {
        run_id: old_run.clone(), intent_id: old_intent.clone(), generation: old_generation,
        owner: "crashed-after-task".into(),
    }).await.expect("release old lease");
    actor.drive_active_refinements().await;
    assert_eq!(
        TaskRepository::new(db.clone(), EventBus::noop())
            .find_by_refinement_intent_id(&old_intent).await.expect("read old task")
            .expect("old task remains").id,
        task_id,
        "task-before-ack replay must retain the original correlated task"
    );

    repo.terminal_refinement_run(TerminalRefinementRunRequest {
        run_id: old_run.clone(), generation: old_generation,
        reason: RefinementStopReason::Interrupted { detail: Some("predecessor fixture".into()) },
    }).await.expect("terminalize predecessor");
    let (current_run, current_generation) = admit_run(&repo, &fixture.proposal_id, "durable-current").await;
    let current_intent = only_intent(&repo, &current_run, current_generation).await;
    actor.drive_active_refinements().await;
    let current_task = TaskRepository::new(db.clone(), EventBus::noop())
        .find_by_refinement_intent_id(&current_intent).await.expect("read current task")
        .expect("current run materialized");
    assert_ne!(current_task.id, task_id);
    assert_eq!(current_task.refinement_run_id.as_deref(), Some(current_run.as_str()));
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
        crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()), cancel.clone(),
        djinn_slot::SlotPoolConfig {
            models: vec![djinn_slot::ModelSlotConfig { model_id: refinement_cap_tests::TEST_MODEL.into(), max_slots: 1,
                roles: ["advocate", "adversary", "judge", "worker"].into_iter().map(str::to_owned).collect() }],
            role_priorities: HashMap::new(),
        },
    );
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, dead_pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation) = admit_run(&repo, &fixture.proposal_id, "durable-enqueue-retry").await;
    let intent_id = only_intent(&repo, &run_id, generation).await;

    actor.drive_active_refinements().await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task_id = task_repo.find_by_refinement_intent_id(&intent_id).await.expect("read failed enqueue task")
        .expect("task is durable despite enqueue failure").id;
    assert_eq!(repo.load_dispatchable_refinement_intents(&run_id, generation).await.expect("read retry intent")[0].state, RefinementIntentState::Materialized);

    actor.pool = refinement_cap_tests::spawn_test_pool(&db, 1);
    actor.drive_active_refinements().await;
    assert_eq!(task_repo.find_by_refinement_intent_id(&intent_id).await.expect("read retried task")
        .expect("same task remains retryable").id, task_id);
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
