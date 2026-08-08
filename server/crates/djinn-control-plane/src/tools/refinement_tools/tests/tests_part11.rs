//! End-to-end PostgreSQL regression for post-cutoff feedback handoff.
//!
//! This intentionally drives both feedback rows through the registered MCP
//! operation.  The only repository transition below is the production terminal
//! lifecycle handoff; the fixture never asks the repository to perform a
//! second artificial feedback capture.

use super::*;
use djinn_core::refinement_liveness::{RefinementPhase, RefinementRole, RefinementStopReason};
use djinn_db::{
    ClaimRefinementIntentRequest, SourceIntentTransitionRequest,
    TerminalRefinementRunFromIntentRequest,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_cutoff_feedback_auto_resume_handoff_captures_the_live_cohort_once() {
    // Production registers the active Advocate/Judge schema providers during
    // agent startup. Activation here therefore exercises the real capability
    // gate rather than bypassing it with a fixture-only switch.
    djinn_agent::init_tool_schema_registry();
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Post-cutoff feedback auto-resume",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: Some("in_review"),
            body_format: None,
        })
        .await
        .unwrap();

    // Caller A commits through the public control-plane operation.  It admits
    // generation N and captures A before returning, establishing the immutable
    // cutoff that caller B must not cross.
    let first = server
        .dispatch_tool(
            "proposal_feedback_add",
            serde_json::json!({
                "proposal_id": proposal.id,
                "body": "A: captured before the cutoff",
                "severity": "blocking",
            }),
        )
        .await
        .unwrap();
    assert!(first["error"].is_null(), "{first}");
    let first_id = first["feedback"]["id"].as_str().unwrap().to_owned();
    let (run_id, intent_id, generation): (String, String, i32) = sqlx::query_as(
        "SELECT r.id, i.id, r.generation FROM refinement_runs r \
         JOIN refinement_dispatch_intents i ON i.run_id=r.id \
         WHERE r.proposal_id=$1 AND r.state='running' AND i.state='pending'",
    )
    .bind(&proposal.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(generation, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM proposal_feedback_refinement_sources WHERE source_feedback_id=$1",
        )
        .bind(&first_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1,
        "A must be immutable before B is permitted to commit",
    );

    // B commits after A's capture has returned while A is still live.  This is
    // the real AlreadyActive auto-resume path: it persists one pending cohort
    // rather than dropping B or manufacturing another immediate run.
    let second = server
        .dispatch_tool(
            "proposal_feedback_add",
            serde_json::json!({
                "proposal_id": proposal.id,
                "body": "B: committed after A's immutable cutoff",
                "severity": "blocking",
            }),
        )
        .await
        .unwrap();
    assert!(second["error"].is_null(), "{second}");
    let second_id = second["feedback"]["id"].as_str().unwrap().to_owned();
    let (pending_members, pending_owners): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE cohort_owner) \
         FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending'",
    )
    .bind(&proposal.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(pending_members, 1, "B must remain pending while A is live");
    assert_eq!(
        pending_owners, 1,
        "B's pending cohort has one elected owner"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM proposal_feedback_refinement_sources WHERE source_feedback_id=$1",
        )
        .bind(&second_id)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        0,
        "B must not leak into A's already immutable capture",
    );

    // Replaying the durable boundaries themselves is harmless and does not
    // create a synthetic feedback row.  This specifically protects the owner
    // election that used to be lost when AlreadyActive was ignored.
    for feedback_id in [&first_id, &second_id] {
        repo.persist_pending_feedback_refinement_handoff(&proposal.id, feedback_id)
            .await
            .unwrap();
    }

    assert!(
        repo.claim_refinement_intent(ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            owner: "post-cutoff-handoff".into(),
            lease_millis: 60_000,
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

    // Advance A through the same production terminal lifecycle transition the
    // coordinator uses.  It must consume B without another feedback caller or
    // an explicit capture call from this test.
    assert!(
        repo.terminal_refinement_run_from_intent(TerminalRefinementRunFromIntentRequest {
            source: transition.clone(),
            reason: RefinementStopReason::OperatorStop {
                actor: "post-cutoff-handoff".into(),
                reason: Some("drain pending B".into()),
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
                    actor: "post-cutoff-handoff".into(),
                    reason: Some("idempotent replay".into()),
                },
            })
            .await
            .unwrap(),
        "replaying lifecycle handoff must not create another successor"
    );

    let (runs, running, intents, objections, immutable_sources, pending, owners):
        (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM refinement_runs WHERE proposal_id=$1), \
           (SELECT count(*) FROM refinement_runs WHERE proposal_id=$1 AND state='running'), \
           (SELECT count(*) FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id=i.run_id WHERE r.proposal_id=$1), \
           (SELECT count(*) FROM proposal_debate_trail WHERE proposal_id=$1 AND kind='human_feedback'), \
           (SELECT count(*) FROM proposal_feedback_refinement_sources WHERE source_feedback_id IN ($2,$3)), \
           (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending'), \
           (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending' AND cohort_owner)",
    )
    .bind(&proposal.id)
    .bind(&first_id)
    .bind(&second_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(runs, 2, "A may produce only one successor generation");
    assert_eq!(running, 1, "only B's successor run remains live");
    assert_eq!(
        intents, 2,
        "replays must not duplicate initial dispatch intents"
    );
    assert_eq!(
        objections, 2,
        "each feedback root has one human-feedback objection"
    );
    assert_eq!(
        immutable_sources, 2,
        "A and B source rows are each captured once"
    );
    assert_eq!(
        pending, 0,
        "the lifecycle handoff drains the pending cohort"
    );
    assert_eq!(owners, 0, "no pending cohort owner survives the drain");

    let (admitted, admitted_owners, b_successor_generation): (i64, i64, i32) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='admitted'), \
           (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='admitted' AND cohort_owner), \
           (SELECT r.generation FROM proposal_feedback_refinement_sources s \
             JOIN proposal_feedback_refinement_injections x ON x.id=s.injection_id \
             JOIN pending_feedback_refinement_handoffs h ON h.boundary_feedback_id=s.source_feedback_id \
             JOIN refinement_runs r ON r.id=h.successor_run_id \
             WHERE s.source_feedback_id=$2)",
    )
    .bind(&proposal.id)
    .bind(&second_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        admitted, 2,
        "A and B boundaries have exactly one admitted owner"
    );
    assert!(
        admitted_owners <= 1,
        "each admitted/captured cohort has at most one durable owner"
    );
    assert_eq!(b_successor_generation, generation + 1);
}
