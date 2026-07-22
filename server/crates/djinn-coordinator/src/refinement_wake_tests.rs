use std::time::Instant as StdInstant;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{
    AdmitRefinementRunRequest, ProposalRepository, RefinementAdmissionOutcome,
    RefinementAdmissionSource,
};

use super::{RefinementSession, refinement_cap_tests};
use crate::refinement::RefinementPhase;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_wake_replays_idempotently_and_rehydrates_disposable_projection() {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = refinement_cap_tests::spawn_test_pool(&db, 1);
    let mut actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());

    let admitted = repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id.clone(),
            idempotency_key: "wake-replay-current".into(),
            source: RefinementAdmissionSource::ExplicitStart {
                actor: fixture.user_id.clone(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit current refinement run");
    let (run_id, generation) = match admitted {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    };
    let lifecycle_before = repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle before wake")
        .len();

    actor.hydrate_refinement_wake(&run_id).await;
    let state = actor
        .active_refinements
        .get_mut(&run_id)
        .expect("current run hydrated under run key");
    state.phase = RefinementPhase::JudgeAdjudication;
    actor.refinement_sessions.insert(
        run_id.clone(),
        RefinementSession {
            run_id: run_id.clone(),
            generation,
            task_id: "current-session".into(),
            phase: RefinementPhase::JudgeAdjudication,
            dispatched_at: StdInstant::now(),
            session_started_at: None,
            model_id: "test/mock".into(),
        },
    );

    actor.hydrate_refinement_wake(&run_id).await;
    assert_eq!(
        actor.active_refinements[&run_id].phase,
        RefinementPhase::JudgeAdjudication,
        "replayed current wake must not reset an advanced projection"
    );
    assert_eq!(
        actor.refinement_sessions[&run_id].task_id,
        "current-session"
    );

    actor.active_refinements.clear();
    actor.refinement_sessions.clear();
    actor.hydrate_refinement_wake(&run_id).await;
    let rehydrated = &actor.active_refinements[&run_id];
    assert_eq!(rehydrated.run_id, run_id);
    assert_eq!(rehydrated.generation, generation);
    assert!(actor.refinement_sessions.is_empty());
    assert_eq!(
        repo.revisions(&fixture.proposal_id)
            .await
            .expect("read lifecycle after wake")
            .len(),
        lifecycle_before,
        "wake replay and rehydration must not create lifecycle rows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_session_generation_is_rejected_for_current_run_projection() {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = refinement_cap_tests::spawn_test_pool(&db, 1);
    let mut actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let admitted = repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id.clone(),
            idempotency_key: "wake-session-fence".into(),
            source: RefinementAdmissionSource::Demand {
                demand_id: "demand-current".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit run");
    let (run_id, generation) = match admitted {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    };
    actor.hydrate_refinement_wake(&run_id).await;
    actor.refinement_sessions.insert(
        run_id.clone(),
        RefinementSession {
            run_id: "older-run".into(),
            generation: generation - 1,
            task_id: "late-session".into(),
            phase: RefinementPhase::AdversaryAttack,
            dispatched_at: StdInstant::now(),
            session_started_at: None,
            model_id: "test/mock".into(),
        },
    );

    actor.drive_active_refinements().await;
    assert!(actor.active_refinements.contains_key(&run_id));
    assert!(!actor.refinement_sessions.contains_key(&run_id));
}
