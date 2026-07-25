//! Focused exact-run recovery matrix.

use super::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use crate::refinement::RefinementPhase;
use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    models::TaskRefinementCorrelation,
    refinement_liveness::{
        RefinementParkKind, RefinementPhase as DurablePhase, RefinementRole, RefinementStopReason,
    },
};
use djinn_db::{
    AcknowledgeRefinementTaskMaterializationRequest, AdmitRefinementRunRequest,
    ClaimRefinementIntentRequest, CompleteRefinementIntentRequest,
    LoadRefinementRunSnapshotRequest, ParkRefinementRunRequest, ProposalRepository,
    RefinementAdmissionOutcome, RefinementAdmissionSource, SessionRepository, TaskRepository,
    TerminalRefinementRunRequest,
};

async fn admit(repo: &ProposalRepository, proposal_id: &str, key: &str) -> (String, i32, String) {
    match repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: proposal_id.into(),
            idempotency_key: key.into(),
            source: RefinementAdmissionSource::ExplicitStart {
                actor: "recovery-test".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit run")
    {
        RefinementAdmissionOutcome::Admitted {
            run_id,
            generation,
            intent_id,
        }
        | RefinementAdmissionOutcome::Existing {
            run_id,
            generation,
            intent_id,
        } => (run_id, generation, intent_id),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hydrates_claimed_run_by_exact_run_id() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 1));
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation, intent_id) =
        admit(&repo, &fixture.proposal_id, "claimed-recovery").await;
    repo.claim_refinement_intent(ClaimRefinementIntentRequest {
        run_id: run_id.clone(),
        intent_id,
        generation,
        owner: "unexpired-recovery-claimer".into(),
        lease_millis: 60_000,
    })
    .await
    .expect("claim intent with unexpired lease")
    .expect("claim acquired");

    actor.recover_interrupted_refinements().await;
    let state = &actor.active_refinements[&run_id];
    assert_eq!(state.run_id, run_id);
    assert_eq!(state.generation, generation);
    assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(state.current_round, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hydrates_materialized_open_task_run_by_exact_run_id() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 1));
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation, intent_id) =
        admit(&repo, &fixture.proposal_id, "materialized-recovery").await;
    let task_id =
        materialize_intent(&actor, &fixture, &repo, &run_id, generation, &intent_id).await;

    actor.recover_interrupted_refinements().await;
    let state = &actor.active_refinements[&run_id];
    assert_eq!(state.run_id, run_id);
    assert_eq!(state.generation, generation);
    assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(state.current_round, 1);
    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .get(&task_id)
        .await
        .expect("read materialized task")
        .expect("materialized task exists");
    assert_eq!(task.status, "open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hydrates_materialized_run_with_active_session_by_exact_run_id() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 1));
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation, intent_id) =
        admit(&repo, &fixture.proposal_id, "session-recovery").await;
    let task_id =
        materialize_intent(&actor, &fixture, &repo, &run_id, generation, &intent_id).await;
    let session = SessionRepository::new(db.clone(), EventBus::noop())
        .create(djinn_db::CreateSessionParams {
            project_id: &fixture.project_id,
            task_id: Some(&task_id),
            model: "test/mock",
            agent_type: "adversary",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("seed active session joined to materialized task");

    actor.recover_interrupted_refinements().await;
    let state = &actor.active_refinements[&run_id];
    assert_eq!(state.run_id, run_id);
    assert_eq!(state.generation, generation);
    assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(state.current_round, 1);
    assert_eq!(session.status, "running");
    assert_eq!(session.task_id.as_deref(), Some(task_id.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hydrates_between_phase_pending_successor_without_session() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 1));
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation, intent_id) =
        admit(&repo, &fixture.proposal_id, "successor-recovery").await;
    let owner = "between-phase-completer".to_owned();
    repo.claim_refinement_intent(ClaimRefinementIntentRequest {
        run_id: run_id.clone(),
        intent_id: intent_id.clone(),
        generation,
        owner: owner.clone(),
        lease_millis: 60_000,
    })
    .await
    .expect("claim source intent")
    .expect("source lease acquired");
    let successor = repo
        .complete_refinement_intent(CompleteRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id,
            generation,
            owner,
            next_round: 2,
            next_phase: DurablePhase::AdvocateRevision,
            next_role: RefinementRole::Advocate,
            next_idempotency_key: format!("{run_id}/2/advocate_revision"),
        })
        .await
        .expect("persist pending successor without session");

    actor.recover_interrupted_refinements().await;
    let state = &actor.active_refinements[&run_id];
    assert_eq!(state.run_id, run_id);
    assert_eq!(state.generation, generation);
    assert_eq!(state.phase, RefinementPhase::AdvocateRevision);
    assert_eq!(state.current_round, successor.round);
    assert_eq!(successor.round, 2);
    assert_eq!(successor.phase, DurablePhase::AdvocateRevision);
    assert_eq!(successor.role, RefinementRole::Advocate);
}

async fn materialize_intent(
    actor: &super::CoordinatorActor,
    fixture: &super::refinement_cap_tests::RefinementFixture,
    repo: &ProposalRepository,
    run_id: &str,
    generation: i32,
    intent_id: &str,
) -> String {
    let owner = "recovery-materializer".to_owned();
    let lease = repo
        .claim_refinement_intent(ClaimRefinementIntentRequest {
            run_id: run_id.into(),
            intent_id: intent_id.into(),
            generation,
            owner: owner.clone(),
            lease_millis: 60_000,
        })
        .await
        .expect("claim recovery intent")
        .expect("acquire recovery lease");
    let correlation = TaskRefinementCorrelation::new(
        run_id.into(),
        intent_id.into(),
        i64::from(generation),
        i64::from(lease.round),
        lease.phase,
        lease.role,
    )
    .expect("valid recovery task correlation");
    let task_id = actor
        .create_refinement_task_with_context_and_correlation(
            &fixture.proposal_id,
            "adversary",
            lease.round,
            0,
            "restart recovery fixture",
            None,
            Some(&fixture.user_id),
            Some(&correlation),
        )
        .await
        .expect("create correlated recovery task");
    assert!(
        repo.acknowledge_refinement_task_materialization(
            AcknowledgeRefinementTaskMaterializationRequest {
                run_id: run_id.into(),
                intent_id: intent_id.into(),
                generation,
                task_id: task_id.clone(),
                owner,
            },
        )
        .await
        .expect("acknowledge recovery task")
    );
    task_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hydrates_pending_run_by_exact_run_id_without_writes_and_replays() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 1));
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation, intent_id) =
        admit(&repo, &fixture.proposal_id, "pending-recovery").await;
    let before = exact_snapshot(&repo, &run_id).await;
    let revision_ids_before = repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle rows before recovery")
        .into_iter()
        .map(|revision| revision.id)
        .collect::<Vec<_>>();

    actor.recover_interrupted_refinements().await;
    let state = actor
        .active_refinements
        .get(&run_id)
        .expect("exact run projection");
    assert_eq!(state.run_id, run_id);
    assert_eq!(state.generation, generation);
    assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(state.current_round, 1);
    assert!(
        before
            .snapshot
            .intents
            .iter()
            .any(|intent| intent.intent_id == intent_id),
        "exact snapshot retains the admitted intent identity"
    );
    actor.active_refinements.clear();
    actor.recover_interrupted_refinements().await;
    assert!(actor.active_refinements.contains_key(&run_id));
    let after = exact_snapshot(&repo, &run_id).await;
    let revision_ids_after = repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle rows after recovery")
        .into_iter()
        .map(|revision| revision.id)
        .collect::<Vec<_>>();
    assert_eq!(
        after.snapshot.intents, before.snapshot.intents,
        "rehydration must not create or mutate durable intents"
    );
    assert_eq!(
        revision_ids_after, revision_ids_before,
        "rehydration is a disposable projection rebuild"
    );
}

async fn exact_snapshot(
    repo: &ProposalRepository,
    run_id: &str,
) -> djinn_db::RefinementRunSnapshotResult {
    repo.load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
        run_id: run_id.into(),
        heartbeat_grace_millis: 60_000,
    })
    .await
    .expect("load exact durable recovery snapshot")
    .expect("admitted run exists")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hydrates_both_parks_and_omits_terminal_exact_runs() {
    for (key, kind, phase) in [
        (
            "review-park",
            RefinementParkKind::AwaitingReview,
            RefinementPhase::AwaitingHumanReview,
        ),
        (
            "evidence-park",
            RefinementParkKind::AwaitingEvidence,
            RefinementPhase::AwaitingEvidence,
        ),
    ] {
        let db = crate::test_helpers::create_test_db();
        let fixture = seed_refinement_fixture(&db).await;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
        let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 1));
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let (run_id, generation, _) = admit(&repo, &fixture.proposal_id, key).await;
        repo.park_refinement_run(ParkRefinementRunRequest {
            run_id: run_id.clone(),
            generation,
            kind,
        })
        .await
        .expect("park exact run");
        actor.recover_interrupted_refinements().await;
        assert_eq!(actor.active_refinements[&run_id].phase, phase);
        repo.terminal_refinement_run(TerminalRefinementRunRequest {
            run_id: run_id.clone(),
            generation,
            reason: RefinementStopReason::OperatorStop {
                actor: "test".into(),
                reason: None,
            },
        })
        .await
        .expect("terminalize exact run");
        actor.active_refinements.clear();
        actor.recover_interrupted_refinements().await;
        assert!(!actor.active_refinements.contains_key(&run_id));
    }
}
