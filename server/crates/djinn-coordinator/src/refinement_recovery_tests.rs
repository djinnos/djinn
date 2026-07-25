//! Focused exact-run recovery matrix.

use super::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use crate::refinement::RefinementPhase;
use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    refinement_liveness::{RefinementParkKind, RefinementStopReason},
};
use djinn_db::{
    AdmitRefinementRunRequest, ParkRefinementRunRequest, ProposalRepository,
    RefinementAdmissionOutcome, RefinementAdmissionSource, TerminalRefinementRunRequest,
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
async fn recovery_hydrates_pending_run_by_exact_run_id_without_writes_and_replays() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 1));
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation, intent_id) =
        admit(&repo, &fixture.proposal_id, "pending-recovery").await;
    let before: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM refinement_dispatch_intents WHERE run_id = $1), \
         (SELECT count(*) FROM proposal_revisions WHERE refinement_run_id = $1)",
    )
    .bind(&run_id)
    .fetch_one(db.pool())
    .await
    .expect("count durable rows");

    actor.recover_interrupted_refinements().await;
    let state = actor
        .active_refinements
        .get(&run_id)
        .expect("exact run projection");
    assert_eq!(state.run_id, run_id);
    assert_eq!(state.generation, generation);
    assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(state.current_round, 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT id FROM refinement_dispatch_intents WHERE id = $1")
            .bind(&intent_id)
            .fetch_one(db.pool())
            .await
            .expect("intent identity survives recovery"),
        intent_id,
    );
    actor.active_refinements.clear();
    actor.recover_interrupted_refinements().await;
    assert!(actor.active_refinements.contains_key(&run_id));
    let after: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM refinement_dispatch_intents WHERE run_id = $1), \
         (SELECT count(*) FROM proposal_revisions WHERE refinement_run_id = $1)",
    )
    .bind(&run_id)
    .fetch_one(db.pool())
    .await
    .expect("count durable rows after replay");
    assert_eq!(
        after, before,
        "rehydration is a disposable projection rebuild"
    );
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
