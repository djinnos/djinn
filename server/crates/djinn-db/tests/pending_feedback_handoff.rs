//! PostgreSQL regressions for durable post-cutoff feedback handoffs.
//!
//! These tests deliberately use the public repository admission, feedback, and
//! lifecycle APIs.  The assertions inspect the durable rows those APIs commit,
//! rather than recreating their SQL in test fixtures.

use djinn_core::events::EventBus;
use djinn_core::refinement_liveness::{RefinementPhase, RefinementRole, RefinementStopReason};
use djinn_db::{
    AdmitRefinementRunRequest, ClaimRefinementIntentRequest, Database, ProposalCreateInput,
    ProposalFeedbackCreateInput, ProposalRepository, RefinementAdmissionOutcome,
    RefinementAdmissionSource, SourceIntentTransitionRequest,
    TerminalRefinementRunFromIntentRequest,
};

const GRACE: i64 = 60_000;

async fn proposal(repo: &ProposalRepository) -> String {
    repo.create(ProposalCreateInput {
        title: "pending feedback handoff",
        body: "body",
        acceptance_criteria: Some("[]"),
        status: Some("in_review"),
        body_format: None,
    })
    .await
    .unwrap()
    .id
}

#[tokio::test]
async fn failed_terminal_handoff_rolls_back_then_retries_without_losing_pending_demand() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&repo).await;
    let (run_id, intent_id, generation) = live_run(&repo, &proposal_id).await;
    let first = blocking_feedback(&repo, &proposal_id, "first rollback boundary").await;
    let second = blocking_feedback(&repo, &proposal_id, "second rollback boundary").await;

    assert!(
        repo.claim_refinement_intent(ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            owner: "terminal-handoff-rollback".into(),
            lease_millis: GRACE,
        })
        .await
        .unwrap()
        .is_some()
    );
    let transition = SourceIntentTransitionRequest {
        run_id: run_id.clone(),
        intent_id: intent_id.clone(),
        generation,
        expected_round: 1,
        expected_phase: RefinementPhase::AdversaryAttack,
        expected_role: RefinementRole::Adversary,
    };

    // This trigger fires only after the production lifecycle has started its
    // source transition and successor admission, so its error exercises the
    // enclosing transaction rather than a precondition failure.
    djinn_db::test_support::reject_refinement_successor_for_test(&db).await;
    let error = repo
        .terminal_refinement_run_from_intent(TerminalRefinementRunFromIntentRequest {
            source: transition.clone(),
            reason: RefinementStopReason::OperatorStop {
                actor: "handoff-regression".into(),
                reason: Some("inject successor failure".into()),
            },
        })
        .await
        .expect_err("injected successor persistence failure must abort the handoff transaction");
    assert!(
        error
            .to_string()
            .contains("injected successor persistence failure")
    );

    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM refinement_runs WHERE id=$1")
            .bind(&run_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "running",
        "the source run terminalization must roll back"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM refinement_dispatch_intents WHERE id=$1",
        )
        .bind(&intent_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        "claimed",
        "the source intent completion must roll back"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_runs WHERE proposal_id=$1 AND generation > $2",
        )
        .bind(&proposal_id)
        .bind(generation)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        0,
        "the failed handoff must not leave a successor run"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id=i.run_id \
             WHERE r.proposal_id=$1 AND r.generation > $2",
        )
        .bind(&proposal_id)
        .bind(generation)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        0,
        "the failed handoff must not leave a successor intent"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM proposal_feedback_refinement_sources WHERE source_feedback_id IN ($1,$2)",
        )
        .bind(&first.id)
        .bind(&second.id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        0,
        "source captures must roll back with successor admission"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pending_feedback_refinement_handoffs \
             WHERE proposal_id=$1 AND state='pending' AND successor_run_id IS NULL",
        )
        .bind(&proposal_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2,
        "every pending boundary must remain durable and unadmitted after failure"
    );

    sqlx::query("DROP TRIGGER reject_refinement_successor_for_test ON refinement_dispatch_intents")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION reject_refinement_successor_for_test()")
        .execute(db.pool())
        .await
        .unwrap();

    assert!(
        repo.terminal_refinement_run_from_intent(TerminalRefinementRunFromIntentRequest {
            source: transition,
            reason: RefinementStopReason::OperatorStop {
                actor: "handoff-regression".into(),
                reason: Some("retry after rollback".into()),
            },
        })
        .await
        .unwrap()
    );

    let successor: (String, i32) = sqlx::query_as(
        "SELECT id, generation FROM refinement_runs WHERE proposal_id=$1 AND state='running' ORDER BY generation",
    )
    .bind(&proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(successor.1, generation + 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_runs WHERE proposal_id=$1 AND state='running'",
        )
        .bind(&proposal_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1,
        "retry must admit exactly one successor run"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_dispatch_intents WHERE run_id=$1 AND state='pending'",
        )
        .bind(&successor.0)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1,
        "retry must create exactly one normal successor intent"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM proposal_feedback_refinement_sources WHERE source_feedback_id IN ($1,$2)",
        )
        .bind(&first.id)
        .bind(&second.id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2,
        "retry captures every boundary exactly once"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pending_feedback_refinement_handoffs \
             WHERE proposal_id=$1 AND state='admitted' AND successor_run_id=$2",
        )
        .bind(&proposal_id)
        .bind(&successor.0)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2,
        "retry admits every retained pending boundary into the one successor"
    );
}

async fn live_run(repo: &ProposalRepository, proposal_id: &str) -> (String, String, i32) {
    match repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: proposal_id.to_owned(),
            idempotency_key: format!("live-run/{proposal_id}"),
            source: RefinementAdmissionSource::ExplicitStart {
                actor: "handoff-regression".into(),
            },
            heartbeat_grace_millis: GRACE,
        })
        .await
        .unwrap()
    {
        RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
        } => (run_id, intent_id, generation),
        other => panic!("expected a fresh live run, got {other:?}"),
    }
}

async fn blocking_feedback(
    repo: &ProposalRepository,
    proposal_id: &str,
    body: &str,
) -> djinn_core::models::ProposalFeedback {
    let (feedback, persisted) = repo
        .add_feedback_with_severity_and_pending_handoff(
            ProposalFeedbackCreateInput {
                proposal_id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body,
            },
            "blocking",
            true,
        )
        .await
        .unwrap();
    assert!(
        persisted,
        "blocking feedback must atomically persist its handoff"
    );
    feedback
}

#[tokio::test]
async fn live_run_feedback_persists_atomically_and_exact_boundary_replay_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&repo).await;
    let _live = live_run(&repo, &proposal_id).await;

    let feedback = blocking_feedback(&repo, &proposal_id, "post-cutoff blocking feedback").await;
    let (boundary, state, owner): (String, String, bool) = sqlx::query_as(
        "SELECT boundary_feedback_id, state, cohort_owner \
         FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1",
    )
    .bind(&proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(boundary, feedback.id);
    assert_eq!(state, "pending");
    assert!(owner);

    let replay = repo
        .persist_pending_feedback_refinement_handoff(&proposal_id, &feedback.id)
        .await
        .unwrap();
    assert_eq!(replay.boundary_feedback_id, feedback.id);
    assert!(replay.cohort_owner);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pending_feedback_refinement_handoffs \
             WHERE proposal_id=$1 AND boundary_feedback_id=$2",
        )
        .bind(&proposal_id)
        .bind(&feedback.id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1,
        "the exact durable boundary must be reused rather than duplicated",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_post_cutoff_boundaries_coalesce_with_one_owner_and_all_members() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&repo).await;
    let _live = live_run(&repo, &proposal_id).await;

    let left_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let right_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let left_proposal = proposal_id.clone();
    let right_proposal = proposal_id.clone();
    let (left, right) = tokio::join!(
        async move { blocking_feedback(&left_repo, &left_proposal, "left boundary").await },
        async move { blocking_feedback(&right_repo, &right_proposal, "right boundary").await },
    );

    let (members, owners): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE cohort_owner) \
         FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending'",
    )
    .bind(&proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        members, 2,
        "every concurrent boundary remains in the cohort"
    );
    assert_eq!(owners, 1, "the pending cohort elects at most one owner");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pending_feedback_refinement_handoffs \
             WHERE proposal_id=$1 AND boundary_feedback_id IN ($2,$3)",
        )
        .bind(&proposal_id)
        .bind(&left.id)
        .bind(&right.id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2,
    );
}

#[tokio::test]
async fn terminal_handoff_creates_one_successor_and_captures_each_source_once_on_retry() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal_id = proposal(&repo).await;
    let (run_id, intent_id, generation) = live_run(&repo, &proposal_id).await;
    let first = blocking_feedback(&repo, &proposal_id, "first pending boundary").await;
    let second = blocking_feedback(&repo, &proposal_id, "second pending boundary").await;

    assert!(
        repo.claim_refinement_intent(ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            owner: "terminal-handoff".into(),
            lease_millis: GRACE,
        })
        .await
        .unwrap()
        .is_some()
    );
    let transition = SourceIntentTransitionRequest {
        run_id: run_id.clone(),
        intent_id: intent_id.clone(),
        generation,
        expected_round: 1,
        expected_phase: RefinementPhase::AdversaryAttack,
        expected_role: RefinementRole::Adversary,
    };
    assert!(
        repo.terminal_refinement_run_from_intent(TerminalRefinementRunFromIntentRequest {
            source: transition.clone(),
            reason: RefinementStopReason::OperatorStop {
                actor: "handoff-regression".into(),
                reason: Some("finish source run".into()),
            },
        })
        .await
        .unwrap()
    );
    assert!(
        !repo
            .terminal_refinement_run_from_intent(TerminalRefinementRunFromIntentRequest {
                source: transition,
                reason: RefinementStopReason::OperatorStop {
                    actor: "handoff-regression".into(),
                    reason: Some("retry".into()),
                },
            })
            .await
            .unwrap()
    );

    let successor: (String, i32) = sqlx::query_as(
        "SELECT id, generation FROM refinement_runs \
         WHERE proposal_id=$1 AND state='running' ORDER BY generation",
    )
    .bind(&proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(successor.1, generation + 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_dispatch_intents WHERE run_id=$1 AND state='pending'",
        )
        .bind(&successor.0)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1,
        "the admitted successor has exactly one normal initial intent",
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pending_feedback_refinement_handoffs \
             WHERE proposal_id=$1 AND state='admitted' AND successor_run_id=$2",
        )
        .bind(&proposal_id)
        .bind(&successor.0)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2,
        "the whole cohort is marked admitted only with its successor",
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM proposal_feedback_refinement_sources \
             WHERE source_feedback_id IN ($1,$2)",
        )
        .bind(&first.id)
        .bind(&second.id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2,
        "each pending feedback boundary has exactly one immutable source capture",
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM refinement_runs WHERE proposal_id=$1 AND state='running'",
        )
        .bind(&proposal_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1,
        "a lifecycle retry cannot create another live successor",
    );
}
