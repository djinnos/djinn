//! End-to-end PostgreSQL regression for post-cutoff feedback handoff.
//!
//! This intentionally drives both feedback rows through the registered MCP
//! operation.  The only repository transition below is the production terminal
//! lifecycle handoff; the fixture never asks the repository to perform a
//! second artificial feedback capture.

use super::*;
use djinn_core::refinement_liveness::{RefinementPhase, RefinementRole, RefinementStopReason};
use djinn_db::{
    ClaimRefinementIntentRequest, RefinementAdmissionSource, SourceIntentTransitionRequest,
    TerminalRefinementRunFromIntentRequest,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_chat_feedback_intent_preserves_spec_and_routes_by_severity() {
    djinn_agent::init_tool_schema_registry();
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Chat feedback must not mutate the spec",
            body: "The original proposal body remains immutable to chat feedback.",
            acceptance_criteria: Some("[]"),
            status: Some("in_review"),
            body_format: None,
        })
        .await
        .unwrap();
    let revision_sequence_before: Vec<_> = repo
        .revisions(&proposal.id)
        .await
        .unwrap()
        .iter()
        .map(|row| row.seq)
        .collect();

    let blocking = server
        .dispatch_tool(
            "proposal_feedback_add",
            serde_json::json!({
                "proposal_id": proposal.id,
                "body": "Blocking chat feedback: this needs tribunal review.",
                "severity": "blocking",
            }),
        )
        .await
        .unwrap();
    assert!(blocking["error"].is_null(), "{blocking}");
    let blocking_id = blocking["feedback"]["id"].as_str().unwrap().to_owned();
    let feedback = repo.feedback(&proposal.id).await.unwrap();
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].severity, "blocking");
    let active = repo
        .load_feedback_refinement_active_boundary(&proposal.id, &blocking_id)
        .await
        .unwrap()
        .expect("blocking chat feedback must start exactly one durable tribunal path");
    assert_eq!(active.generation, 1);
    assert_eq!(active.source_captures, 1);

    let advisory = server
        .dispatch_tool(
            "proposal_feedback_add",
            serde_json::json!({
                "proposal_id": proposal.id,
                "body": "Advisory chat feedback: optional follow-up.",
                "severity": "advisory",
            }),
        )
        .await
        .unwrap();
    assert!(advisory["error"].is_null(), "{advisory}");
    let advisory_id = advisory["feedback"]["id"].as_str().unwrap().to_owned();
    let feedback = repo.feedback(&proposal.id).await.unwrap();
    assert_eq!(feedback.len(), 2);
    assert_eq!(feedback[1].severity, "advisory");
    let advisory_state = repo
        .load_pending_feedback_refinement_state(&proposal.id, &advisory_id)
        .await
        .unwrap();
    assert_eq!(
        (
            advisory_state.pending_members,
            advisory_state.pending_owners,
            advisory_state.source_captures
        ),
        (0, 0, 0)
    );

    let revisions_after = repo.revisions(&proposal.id).await.unwrap();
    assert_eq!(
        revisions_after
            .iter()
            .map(|row| row.seq)
            .collect::<Vec<_>>(),
        revision_sequence_before
    );
    let proposal_after = repo.resolve(&proposal.id).await.unwrap().unwrap();
    assert_eq!(proposal_after.body, proposal.body);
    assert_eq!(
        proposal_after.latest_revision_seq,
        proposal.latest_revision_seq
    );
}

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
    let active = repo
        .load_feedback_refinement_active_boundary(&proposal.id, &first_id)
        .await
        .unwrap()
        .expect("A's feedback admission remains live");
    let run_id = active.run_id;
    let intent_id = active.intent_id;
    let generation = active.generation;
    assert_eq!(generation, 1);
    assert_eq!(
        active.source_captures, 1,
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
    let pending_state = repo
        .load_pending_feedback_refinement_state(&proposal.id, &second_id)
        .await
        .unwrap();
    assert_eq!(
        pending_state.pending_members, 1,
        "B must remain pending while A is live"
    );
    assert_eq!(
        pending_state.pending_owners, 1,
        "B's pending cohort has one elected owner"
    );
    assert_eq!(
        pending_state.source_captures, 0,
        "B must not leak into A's already immutable capture",
    );

    // Replay the actual admission boundaries with the exact identities the
    // public feedback operation used. A's committed boundary must resolve to
    // Existing, while B must still report the live A run as AlreadyActive; no
    // replay is allowed to manufacture another demand intent or run.
    let replayed_a = crate::tools::refinement_tools::admit_refinement_run(
        &server,
        &repo,
        &proposal.id,
        RefinementAdmissionSource::Demand {
            demand_id: format!("feedback:auto-resume:boundary:{first_id}"),
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(replayed_a.run_id, run_id);
    assert!(
        !replayed_a.admitted,
        "replaying A's boundary must resolve to its existing admission"
    );
    let replayed_b = crate::tools::refinement_tools::admit_refinement_run(
        &server,
        &repo,
        &proposal.id,
        RefinementAdmissionSource::Demand {
            demand_id: format!("feedback:auto-resume:boundary:{second_id}"),
        },
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        replayed_b.code, "already_active",
        "B's replay must retain the live-run auto-resume path"
    );

    // Replaying the durable handoff boundaries themselves is harmless and does
    // not create a synthetic feedback row. This specifically protects the
    // owner election that used to be lost when AlreadyActive was ignored.
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

    let lifecycle = repo
        .load_feedback_refinement_lifecycle_state(&proposal.id, &first_id, &second_id)
        .await
        .unwrap();
    assert_eq!(
        lifecycle.runs, 2,
        "A may produce only one successor generation"
    );
    assert_eq!(lifecycle.running, 1, "only B's successor run remains live");
    assert_eq!(
        lifecycle.intents, 2,
        "replays must not duplicate initial dispatch intents"
    );
    assert_eq!(
        lifecycle.objections, 2,
        "each feedback root has one human-feedback objection"
    );
    assert_eq!(
        lifecycle.injections, 2,
        "the schedule has no queued or empty immutable injection generation"
    );
    assert_eq!(
        lifecycle.immutable_generations, 2,
        "A and B are the schedule's only immutable feedback generations"
    );
    assert_eq!(
        lifecycle.immutable_sources, 2,
        "A and B source rows are each captured once"
    );
    assert_eq!(
        lifecycle.pending, 0,
        "the lifecycle handoff drains the pending cohort"
    );
    assert_eq!(
        lifecycle.pending_owners, 0,
        "no pending cohort owner survives the drain"
    );

    let admitted_state = repo
        .load_feedback_refinement_admitted_state(&proposal.id, &second_id)
        .await
        .unwrap();
    assert_eq!(
        admitted_state.admitted, 2,
        "A and B boundaries have exactly one admitted owner"
    );
    assert!(
        admitted_state.admitted_owners <= 1,
        "each admitted/captured cohort has at most one durable owner"
    );
    assert_eq!(admitted_state.successor_generation, Some(generation + 1));
}
