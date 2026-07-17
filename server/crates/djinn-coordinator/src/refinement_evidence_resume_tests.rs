// djinn:allow-oversize
// djinn:allow-oversize
use crate::refinement::RefinementPhase;
use crate::refinement_dispatch::refinement_cap_tests::{
    TEST_MODEL, build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{
    EffectiveCreatorProvenance, ProposalDebateTrailCreateInput, ProposalRepository, TaskRepository,
};

/// Successful evidence receipt clears the linked spike/claim and resumes with
/// the next Advocate task, carrying the findings adjacent to proposal/debate context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_receipt_clears_link_and_dispatches_advocate_with_findings_context() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    let spike_task_id = task_repo
        .create_in_project_with_provenance(
            &fixture.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&fixture.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "Evidence spike",
            "Investigate the load-bearing claim",
            "",
            "spike",
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create spike task")
        .id;
    let claim = r#"{"question":"Can the coordinator resume safely?","target_subsystem":"refinement","spec_unknown_anchor":"resume path","insufficient_in_session_research":"needs spike","expected_findings":"structured findings","round":1,"against_revision_seq":1,"created_by_task_id":"judge-task"}"#;
    proposal_repo
        .set_needs_evidence_spike(&fixture.proposal_id, &spike_task_id, claim)
        .await
        .expect("link spike");
    proposal_repo
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &fixture.proposal_id,
            kind: "objection",
            body: "OBJECTION-NEEDS-EVIDENCE-019f0c22",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: Some(TEST_MODEL),
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: None,
        })
        .await
        .expect("record objection");
    let findings_metadata = serde_json::json!({
        "answer": "FINDINGS-ANSWER-019f0c22 resume is safe",
        "evidence": ["terminal spike completed with valid handoff"],
        "code_paths_inspected": ["server/crates/djinn-coordinator/src/refinement_dispatch.rs"],
        "confidence": 0.91,
        "residual_risks": ["restart recovery owned by sibling task"],
        "recommendation_for_advocate": "Use the findings to update the proposal"
    });
    proposal_repo
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &fixture.proposal_id,
            kind: "evidence_findings",
            body: "FINDINGS-BODY-019f0c22",
            blocking: false,
            agent_role: "spike",
            author_kind: "agent",
            author_model: Some(TEST_MODEL),
            source_task_id: Some(&spike_task_id),
            against_revision_seq: 1,
            round: 1,
            body_metadata: Some(&findings_metadata),
        })
        .await
        .expect("record evidence findings");
    task_repo
        .set_status_with_reason(&spike_task_id, "closed", Some("completed"))
        .await
        .expect("close spike completed");

    let task = task_repo
        .get(&spike_task_id)
        .await
        .expect("read spike")
        .expect("spike exists");
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    crate::refinement_dispatch::refinement_cap_tests::seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    actor
        .persist_terminal_linked_spike_evidence_from_closed_task(&task)
        .await;

    let updated = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert!(updated.linked_spike_task_id.is_none());
    assert!(updated.needs_evidence_claim.is_none());
    let session = actor
        .refinement_sessions
        .get(&fixture.proposal_id)
        .expect("Advocate session dispatched");
    assert_eq!(session.phase, RefinementPhase::AdvocateRevision);
    let advocate_task = task_repo
        .get(&session.task_id)
        .await
        .expect("read advocate task")
        .expect("advocate task exists");
    assert!(
        advocate_task
            .description
            .contains("Evidence findings received")
    );
    assert!(advocate_task.description.contains("FINDINGS-BODY-019f0c22"));
    assert!(
        advocate_task
            .description
            .contains("FINDINGS-ANSWER-019f0c22")
    );
    assert!(
        advocate_task
            .description
            .contains("OBJECTION-NEEDS-EVIDENCE-019f0c22")
    );
}

/// When a manual freeze gate is active, evidence receipt is recorded and the
/// link is cleared, but no Advocate task is auto-dispatched until the gate clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_receipt_respects_freeze_without_auto_dispatch() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    let spike_task_id = task_repo
        .create_in_project_with_provenance(
            &fixture.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&fixture.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "Frozen evidence spike",
            "Investigate while frozen",
            "",
            "spike",
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create spike task")
        .id;
    let claim = r#"{"question":"Can frozen resume wait?","target_subsystem":"refinement","spec_unknown_anchor":"freeze gate","insufficient_in_session_research":"needs spike","expected_findings":"structured findings","round":1,"against_revision_seq":1,"created_by_task_id":"judge-task"}"#;
    proposal_repo
        .set_needs_evidence_spike(&fixture.proposal_id, &spike_task_id, claim)
        .await
        .expect("link spike");
    let findings_metadata = serde_json::json!({
        "answer": "freeze gate finding",
        "evidence": ["valid while frozen"],
        "code_paths_inspected": ["server/crates/djinn-coordinator/src/refinement_dispatch.rs"],
        "confidence": 0.8,
        "residual_risks": ["none for freeze test"],
        "recommendation_for_advocate": "wait for the gate"
    });
    proposal_repo
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &fixture.proposal_id,
            kind: "evidence_findings",
            body: "frozen findings",
            blocking: false,
            agent_role: "spike",
            author_kind: "agent",
            author_model: Some(TEST_MODEL),
            source_task_id: Some(&spike_task_id),
            against_revision_seq: 1,
            round: 1,
            body_metadata: Some(&findings_metadata),
        })
        .await
        .expect("record findings");
    proposal_repo
        .set_frozen(&fixture.proposal_id, true)
        .await
        .expect("freeze proposal");
    task_repo
        .set_status_with_reason(&spike_task_id, "closed", Some("completed"))
        .await
        .expect("close spike completed");
    let task = task_repo
        .get(&spike_task_id)
        .await
        .expect("read spike")
        .expect("spike exists");
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    crate::refinement_dispatch::refinement_cap_tests::seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    actor
        .persist_terminal_linked_spike_evidence_from_closed_task(&task)
        .await;

    let updated = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert!(updated.linked_spike_task_id.is_none());
    assert!(updated.needs_evidence_claim.is_none());
    assert!(actor.refinement_sessions.is_empty());
    let tasks = task_repo
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    assert!(
        tasks.iter().all(|task| task.issue_type != "refinement"),
        "freeze gate must prevent Advocate task creation"
    );
}
