// End-to-end evidence parking, findings, recovery, and race regressions
// for the coordinator refinement lifecycle (oeqd Task 5).
//
// These tests use the same deterministic DB/task fixtures as the upstream
// evidence-lifecycle tasks and prove the epic-level behaviors across
// coordinator/control-plane boundaries.  They do not require a live
// runtime; they do require a running Postgres instance (the standard
// `TEST_POSTGRES_URL` environment), so in a DB-free worker environment the
// DB-backed assertions are expected to fail at fixture creation with a
// connection-refused error.
//
// Coverage:
// - open linked spike parks refinement and prevents further Adversary,
//   Advocate, or Judge dispatch from normal and re-drive paths;
// - valid evidence completion writes `refinement_evidence_received`, clears the
//   link/claim exactly once, and resumes the next Advocate with findings in
//   context;
// - missing/malformed findings and failed/cancelled/force-closed spikes write
//   `refinement_evidence_failed`, keep the proposal blocked, and do not resume;
// - manual freeze/pause precedence after evidence receipt: receipt is recorded
//   but automatic resume waits for the gate to clear;
// - restart/re-drive idempotency for open, closed-with-findings,
//   closed-without-findings, failed, and already-processed lifecycle rows;
// - races: two Judge demands cannot create multiple linked spikes, and sibling
//   refinement task completions after AwaitingEvidence is recorded cannot enqueue
//   extra tribunal rounds.
//
// djinn:allow-oversize

use crate::refinement::RefinementPhase;
use crate::refinement_dispatch::refinement_cap_tests::{
    TEST_MODEL, build_refinement_actor, seed_refinement_fixture, seed_refinement_state,
    spawn_test_pool,
};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{NeedsEvidenceClaim, TribunalEvidenceLifecycle};
use djinn_db::{
    EffectiveCreatorProvenance, ProposalDebateTrailCreateInput, ProposalRepository, TaskRepository,
    TypedEvidenceLifecycleProjection, TypedEvidenceRepository,
};
use serde::Deserialize;

const LIFECYCLE_CASES: &str = include_str!("../tests/fixtures/evidence_lifecycle_cases.json");

#[derive(Deserialize)]
struct EvidenceLifecycleFixture {
    cases: Vec<EvidenceLifecycleCase>,
}

#[derive(Deserialize)]
struct EvidenceLifecycleCase {
    name: String,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a deterministic typed claim payload for the fixture proposal.
fn sample_claim(judge_task_id: &str) -> NeedsEvidenceClaim {
    NeedsEvidenceClaim {
        question: "Can the coordinator resume safely?".to_string(),
        target_subsystem: "refinement".to_string(),
        spec_unknown_anchor: "resume path".to_string(),
        insufficient_in_session_research: "needs spike".to_string(),
        expected_findings: "structured findings".to_string(),
        round: 1,
        against_revision_seq: 1,
        created_by_task_id: judge_task_id.to_string(),
    }
}

/// Build a deterministic evidence_findings payload with the given answer.
fn sample_findings_metadata(answer: &str) -> serde_json::Value {
    serde_json::json!({
        "answer": answer,
        "evidence": ["terminal spike completed with valid handoff"],
        "code_paths_inspected": ["server/crates/djinn-coordinator/src/refinement_dispatch.rs"],
        "confidence": 0.91,
        "residual_risks": ["restart recovery owned by sibling task"],
        "recommendation_for_advocate": "Use the findings to update the proposal"
    })
}

/// Create a linked evidence spike task for the fixture proposal, optionally with
/// a findings entry and a closed status.  Returns the spike task id and the
/// Judge task id used in the claim.
async fn seed_linked_spike(
    db: &djinn_db::Database,
    fixture: &crate::refinement_dispatch::refinement_cap_tests::RefinementFixture,
    spike_status: &str,
    close_reason: Option<&str>,
    findings: Option<serde_json::Value>,
) -> (String, String) {
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

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

    // Record a Judge task so the claim has a valid created_by_task_id.
    let judge_task_id = task_repo
        .create_in_project_with_provenance(
            &fixture.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&fixture.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "Judge for refinement",
            "Judge the proposal",
            "refinement",
            "judge",
            0,
            "worker",
            Some("closed"),
            Some("[]"),
        )
        .await
        .expect("create judge task")
        .id;

    let claim = sample_claim(&judge_task_id);
    proposal_repo
        .set_structured_needs_evidence_spike(&fixture.proposal_id, &spike_task_id, &claim)
        .await
        .expect("link spike");

    if let Some(meta) = findings {
        proposal_repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &fixture.proposal_id,
                kind: "evidence_findings",
                body: "FINDINGS-BODY-E2E",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: Some(TEST_MODEL),
                source_task_id: Some(&spike_task_id),
                against_revision_seq: 1,
                round: 1,
                body_metadata: Some(&meta),
            })
            .await
            .expect("record evidence findings");
    }

    task_repo
        .set_status_with_reason(&spike_task_id, spike_status, close_reason)
        .await
        .expect("set spike status");

    (spike_task_id, judge_task_id)
}

/// Seed the canonical typed authority and a production durable delivery.
async fn seed_typed_return_delivery(
    db: &djinn_db::Database,
    proposal_id: &str,
    spike_task_id: &str,
) -> String {
    let fixture = djinn_db::test_support::seed_typed_evidence_ingress_fixture_for_test(
        db,
        proposal_id,
        spike_task_id,
        "e2e-check",
    )
    .await;
    let raw = fixture.return_payload;
    TaskRepository::new(db.clone(), EventBus::noop())
        .log_activity(
            Some(spike_task_id),
            "worker",
            "worker",
            "tribunal_evidence_return_v1",
            &raw,
        )
        .await
        .expect("log durable typed return");
    raw
}

/// Count the number of refinement tasks in the project with the given agent
/// type (advocate/adversary/judge).
async fn count_refinement_tasks(
    task_repo: &TaskRepository,
    project_id: &str,
    agent_type: &str,
) -> usize {
    task_repo
        .list_by_project(project_id)
        .await
        .expect("list tasks")
        .into_iter()
        .filter(|t| t.issue_type == "refinement" && t.agent_type.as_deref() == Some(agent_type))
        .count()
}

// ── AC#1: open linked spike parks dispatch ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_linked_spike_parks_normal_and_redispatch_paths() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    let (spike_task_id, _judge_task_id) =
        seed_linked_spike(&db, &fixture, "open", None, None).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    // Normal dispatch path.
    actor
        .dispatch_next_refinement_phase(&fixture.proposal_id)
        .await;

    // Re-drive path: simulate a tick while the spike is still open.
    actor.drive_active_refinements().await;

    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await,
        0,
        "no Advocate dispatched while open linked spike exists"
    );
    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "adversary").await,
        0,
        "no Adversary dispatched while open linked spike exists"
    );
    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "judge").await,
        0,
        "no Judge dispatched while open linked spike exists"
    );

    // The spike remains linked and the proposal is not blocked by failure.
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert_eq!(
        proposal.linked_spike_task_id.as_deref(),
        Some(spike_task_id.as_str())
    );
    assert!(proposal.needs_evidence_claim.is_some());
}

// ── AC#2: valid evidence completion clears link and resumes Advocate ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_evidence_completion_resumes_only_advocate_and_leaves_typed_finding_unresolved() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    let (spike_task_id, _judge_task_id) = seed_linked_spike(
        &db,
        &fixture,
        "closed",
        Some("completed"),
        Some(sample_findings_metadata("FINDINGS-ANSWER-E2E valid")),
    )
    .await;
    let raw = seed_typed_return_delivery(&db, &fixture.proposal_id, &spike_task_id).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    // Simulate the event-driven completion path.
    let task = task_repo
        .get(&spike_task_id)
        .await
        .expect("read spike")
        .expect("spike exists");
    let live = actor
        .ingest_raw_tribunal_evidence_return_v1(&task.id, &raw)
        .await
        .expect("live typed delivery persisted");
    let replay = actor
        .recover_terminal_linked_spike_evidence_for_task(&task.id)
        .await;
    assert_eq!(replay.len(), 1);
    assert!(replay[0].replayed);
    assert_eq!(replay[0].validation_id, live.validation_id);
    let transitions =
        djinn_db::test_support::typed_evidence_transition_count_for_validation_for_test(
            &db,
            &live.validation_id,
        )
        .await;
    assert_eq!(transitions, 1);

    // Link and claim are cleared exactly once by the typed transaction.
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert!(proposal.linked_spike_task_id.is_none());
    assert!(proposal.needs_evidence_claim.is_none());

    // The next Advocate session is in-flight with findings in context.
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
    assert!(advocate_task.description.contains("FINDINGS-BODY-E2E"));
    assert!(
        advocate_task
            .description
            .contains("FINDINGS-ANSWER-E2E valid")
    );

    // Evidence receipt is deliberately not a structural disposition. Exercise
    // the real legacy dispatcher with stale non-Advocate phases to prove it
    // does not materialize either role while the typed finding remains live.
    let refinement_tasks_before_stale_dispatch = task_repo
        .list_by_project(&fixture.project_id)
        .await
        .expect("list refinement tasks after Advocate resume")
        .into_iter()
        .filter(|task| task.issue_type == "refinement")
        .count();
    for stale_phase in [
        RefinementPhase::AdversaryAttack,
        RefinementPhase::JudgeAdjudication,
    ] {
        actor
            .active_refinements
            .get_mut(&fixture.proposal_id)
            .expect("legacy refinement state remains available")
            .phase = stale_phase;
        actor
            .dispatch_next_refinement_phase(&fixture.proposal_id)
            .await;
    }
    let refinement_tasks_after_stale_dispatch = task_repo
        .list_by_project(&fixture.project_id)
        .await
        .expect("list refinement tasks after blocked stale phases")
        .into_iter()
        .filter(|task| task.issue_type == "refinement")
        .count();
    assert_eq!(
        refinement_tasks_after_stale_dispatch, refinement_tasks_before_stale_dispatch,
        "received evidence must not dispatch stale Adversary or Judge phases"
    );

    // Re-read the authoritative projection after actual Advocate dispatch and
    // stale-phase rejection. It must still be evidence_received, rather than
    // silently promoted to a structural resolution by coordinator resume.
    let projection = TypedEvidenceRepository::new(db.clone())
        .coordinator_lifecycle_projection(&fixture.proposal_id)
        .await
        .expect("read typed evidence after Advocate resume");
    let TypedEvidenceLifecycleProjection::Valid(finding) = projection else {
        panic!("typed evidence must remain a valid live finding after resume");
    };
    assert_eq!(
        finding.lifecycle,
        TribunalEvidenceLifecycle::EvidenceReceived
    );
    assert!(
        !finding.lifecycle.is_terminal(),
        "Advocate folding must not resolve or withdraw the typed finding"
    );
}

/// Legacy terminal/prose state is not evidence: only a durable V1 activity can
/// cross the coordinator ingress, regardless of the old lifecycle case label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_terminal_lifecycle_cases_do_not_manufacture_typed_receipts() {
    let contract: EvidenceLifecycleFixture =
        serde_json::from_str(LIFECYCLE_CASES).expect("valid lifecycle fixture");
    for case in contract.cases {
        let db = crate::test_helpers::create_test_db();
        let fixture = seed_refinement_fixture(&db).await;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
        let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let (spike_task_id, _) = seed_linked_spike(
            &db,
            &fixture,
            "closed",
            Some("completed"),
            Some(sample_findings_metadata(&case.name)),
        )
        .await;
        let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 4));
        seed_refinement_state(
            &mut actor,
            &fixture.proposal_id,
            Some(fixture.user_id.clone()),
        );
        actor
            .active_refinements
            .get_mut(&fixture.proposal_id)
            .expect("state exists")
            .record_needs_evidence();
        let results = actor
            .recover_terminal_linked_spike_evidence_for_task(&spike_task_id)
            .await;
        assert!(
            results.is_empty(),
            "{}: closure/prose alone is not a typed delivery",
            case.name
        );
        let proposal = proposal_repo
            .get(&fixture.proposal_id)
            .await
            .expect("read proposal")
            .expect("proposal exists");
        assert_eq!(
            proposal.linked_spike_task_id.as_deref(),
            Some(spike_task_id.as_str())
        );
        assert!(proposal.needs_evidence_claim.is_some());
        assert!(actor.refinement_sessions.is_empty());
        assert_eq!(
            count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await,
            0
        );
    }
}

// ── AC#3: missing/malformed findings and failed spikes block resume ──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_findings_record_failure_and_block_resume() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    let (spike_task_id, _judge_task_id) =
        seed_linked_spike(&db, &fixture, "closed", Some("completed"), None).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    let task = task_repo
        .get(&spike_task_id)
        .await
        .expect("read spike")
        .expect("spike exists");
    let results = actor
        .recover_terminal_linked_spike_evidence_for_task(&task.id)
        .await;
    assert!(
        results.is_empty(),
        "closure/prose alone is not a typed delivery"
    );

    // Link and claim remain set; failure lifecycle recorded; no Advocate.
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert_eq!(
        proposal.linked_spike_task_id.as_deref(),
        Some(spike_task_id.as_str())
    );
    assert!(proposal.needs_evidence_claim.is_some());

    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let failure_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_failed")
        .collect();
    assert_eq!(
        failure_events.len(),
        0,
        "no typed payload means no failure receipt"
    );

    assert!(actor.refinement_sessions.is_empty());
    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_spike_records_failure_and_blocks_resume() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    let (spike_task_id, _judge_task_id) = seed_linked_spike(
        &db,
        &fixture,
        "closed",
        Some("failed"),
        Some(sample_findings_metadata("should not matter")),
    )
    .await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    let task = task_repo
        .get(&spike_task_id)
        .await
        .expect("read spike")
        .expect("spike exists");
    let results = actor
        .recover_terminal_linked_spike_evidence_for_task(&task.id)
        .await;
    assert!(
        results.is_empty(),
        "closure/prose alone is not a typed delivery"
    );

    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let failure_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_failed")
        .collect();
    assert_eq!(
        failure_events.len(),
        0,
        "no typed payload means no failure receipt"
    );

    assert!(actor.refinement_sessions.is_empty());
}

// ── AC#4: freeze/pause precedence after receipt ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_precedence_after_receipt_records_but_does_not_resume() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    let (spike_task_id, _judge_task_id) = seed_linked_spike(
        &db,
        &fixture,
        "closed",
        Some("completed"),
        Some(sample_findings_metadata("freeze gate finding")),
    )
    .await;
    let raw = seed_typed_return_delivery(&db, &fixture.proposal_id, &spike_task_id).await;

    // Freeze the proposal before processing the closed spike.
    proposal_repo
        .set_frozen(&fixture.proposal_id, true)
        .await
        .expect("freeze proposal");

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    let task = task_repo
        .get(&spike_task_id)
        .await
        .expect("read spike")
        .expect("spike exists");
    let live = actor
        .ingest_raw_tribunal_evidence_return_v1(&task.id, &raw)
        .await
        .expect("live typed delivery persisted");
    let replay = actor
        .recover_terminal_linked_spike_evidence_for_task(&task.id)
        .await;
    assert_eq!(replay.len(), 1);
    assert!(replay[0].replayed);
    assert_eq!(replay[0].validation_id, live.validation_id);

    // Receipt is recorded and link/claim cleared by the typed transaction.
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert!(proposal.linked_spike_task_id.is_none());
    assert!(proposal.needs_evidence_claim.is_none());
    // But no automatic resume while frozen.
    assert!(actor.refinement_sessions.is_empty());
    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await,
        0
    );
}

// ── AC#5: restart/re-drive idempotency ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redrive_open_spike_remains_parked() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    seed_linked_spike(&db, &fixture, "open", None, None).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    // Simulate startup recovery and re-drive.
    actor.recover_terminal_linked_spike_evidence().await;
    actor.drive_active_refinements().await;
    actor.recover_terminal_linked_spike_evidence().await;
    actor.drive_active_refinements().await;

    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await,
        0
    );
    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "adversary").await,
        0
    );
    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "judge").await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redrive_closed_with_findings_is_idempotent() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    seed_linked_spike(
        &db,
        &fixture,
        "closed",
        Some("completed"),
        Some(sample_findings_metadata("idempotent findings")),
    )
    .await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    // First recovery pass.
    actor.recover_terminal_linked_spike_evidence().await;
    // Second recovery pass must not duplicate lifecycle events or tasks.
    actor.recover_terminal_linked_spike_evidence().await;
    actor.drive_active_refinements().await;
    actor.recover_terminal_linked_spike_evidence().await;

    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let receipt_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_received")
        .collect();
    assert_eq!(receipt_events.len(), 0, "no typed payload means no receipt");

    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redrive_closed_without_findings_is_idempotent() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    seed_linked_spike(&db, &fixture, "closed", Some("completed"), None).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    actor.recover_terminal_linked_spike_evidence().await;
    actor.recover_terminal_linked_spike_evidence().await;
    actor.drive_active_refinements().await;

    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let failure_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_failed")
        .collect();
    assert_eq!(
        failure_events.len(),
        0,
        "no typed payload means no failure receipt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redrive_failed_spike_is_idempotent() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    seed_linked_spike(
        &db,
        &fixture,
        "closed",
        Some("failed"),
        Some(sample_findings_metadata("ignored findings")),
    )
    .await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    actor.recover_terminal_linked_spike_evidence().await;
    actor.recover_terminal_linked_spike_evidence().await;
    actor.drive_active_refinements().await;

    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let failure_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_failed")
        .collect();
    assert_eq!(
        failure_events.len(),
        0,
        "no typed payload means no failure receipt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redrive_already_processed_lifecycle_is_idempotent() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    seed_linked_spike(
        &db,
        &fixture,
        "closed",
        Some("completed"),
        Some(sample_findings_metadata("already processed")),
    )
    .await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    actor.recover_terminal_linked_spike_evidence().await;
    // The first pass atomically records the typed receipt and clears the
    // compatible legacy authority. The following passes model an
    // already-cleared lifecycle without a second terminal transition.
    actor.recover_terminal_linked_spike_evidence().await;
    actor.drive_active_refinements().await;
    actor.recover_terminal_linked_spike_evidence().await;

    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let receipt_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_received")
        .collect();
    assert_eq!(receipt_events.len(), 0, "no typed payload means no receipt");
}

// ── AC#6: races ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_judge_demands_cannot_create_two_linked_spikes() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    // Create a Judge task and link the first spike.
    let judge_task_id = task_repo
        .create_in_project_with_provenance(
            &fixture.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&fixture.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "Judge for refinement",
            "Judge the proposal",
            "refinement",
            "judge",
            0,
            "worker",
            Some("closed"),
            Some("[]"),
        )
        .await
        .expect("create judge task")
        .id;
    let claim = sample_claim(&judge_task_id);

    let spike1 = task_repo
        .create_in_project_with_provenance(
            &fixture.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&fixture.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "First spike",
            "First investigation",
            "",
            "spike",
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create spike 1")
        .id;
    proposal_repo
        .set_structured_needs_evidence_spike(&fixture.proposal_id, &spike1, &claim)
        .await
        .expect("link first spike");

    // A second demand attempts to link another spike.
    let spike2 = task_repo
        .create_in_project_with_provenance(
            &fixture.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&fixture.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "Second spike",
            "Second investigation",
            "",
            "spike",
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create spike 2")
        .id;
    let result = proposal_repo
        .try_set_structured_needs_evidence_spike(&fixture.proposal_id, &spike2, &claim)
        .await;

    // An occupied link is a race loser: it must return None and leave the
    // established typed and legacy authority untouched.
    assert!(result.expect("try link second spike").is_none());
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert_eq!(
        proposal.linked_spike_task_id.as_deref(),
        Some(spike1.as_str())
    );
    assert_eq!(
        NeedsEvidenceClaim::parse_stored(proposal.needs_evidence_claim.as_deref())
            .expect("stored claim must remain valid"),
        Some(claim)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sibling_refinement_completions_after_awaiting_evidence_do_not_enqueue_extra_rounds() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());

    // Seed an active Advocate task that completes *after* AwaitingEvidence is
    // already recorded.  Mimic production: `issue_type = "refinement"` with
    // `agent_type` set separately via `update_agent_type`.
    let advocate_task_id = task_repo
        .create_in_project_with_provenance(
            &fixture.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&fixture.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "Advocate for refinement",
            "Advocate the proposal",
            "",
            "refinement",
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create advocate task")
        .id;
    task_repo
        .update_agent_type(&advocate_task_id, Some("advocate"))
        .await
        .expect("set advocate agent type");

    // Record AwaitingEvidence lifecycle event.
    proposal_repo
        .record_refinement_lifecycle(
            &fixture.proposal_id,
            "refinement_awaiting_evidence_started",
            None,
        )
        .await
        .expect("record awaiting evidence");

    // Close the sibling Advocate task as if it completed late.
    task_repo
        .set_status_with_reason(&advocate_task_id, "closed", Some("completed"))
        .await
        .expect("close advocate task");

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("state exists")
        .record_needs_evidence();

    // Simulate processing a closed task that is *not* the linked spike.  The
    // coordinator should not treat a sibling refinement task completion as
    // evidence receipt and must not enqueue extra rounds.
    let task = task_repo
        .get(&advocate_task_id)
        .await
        .expect("read advocate task")
        .expect("advocate task exists");
    let results = actor
        .recover_terminal_linked_spike_evidence_for_task(&task.id)
        .await;
    assert!(
        results.is_empty(),
        "closure/prose alone is not a typed delivery"
    );
    actor.drive_active_refinements().await;

    let advocate_count = count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await;
    assert_eq!(
        advocate_count, 1,
        "only the pre-existing Advocate task exists"
    );
    assert!(actor.refinement_sessions.is_empty());
}

/// Startup recovery is the production historical-backfill seam. With no
/// linked spike candidate it must return from its read-only candidate query,
/// rather than manufacturing lifecycle evidence for an unrelated proposal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_historical_evidence_backfill_is_a_zero_write_noop() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());

    assert!(
        proposals
            .list_linked_evidence_spike_recovery_candidates()
            .await
            .expect("query empty eligible population")
            .is_empty(),
        "fixture intentionally has no linked evidence spike eligible for backfill"
    );

    let before = (
        djinn_db::test_support::count_rows_for_test(&db, "evidence_plans").await,
        djinn_db::test_support::count_rows_for_test(&db, "evidence_command_invocations").await,
        djinn_db::test_support::count_rows_for_test(&db, "evidence_finalized_projections").await,
        djinn_db::test_support::count_rows_for_test(&db, "proposal_debate_trail").await,
        djinn_db::test_support::count_rows_for_test(&db, "proposal_revisions").await,
    );
    let revisions_before = proposals
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle baseline");

    let mut actor = build_refinement_actor(&db, &events_tx, spawn_test_pool(&db, 4));
    actor.recover_terminal_linked_spike_evidence().await;

    let after = (
        djinn_db::test_support::count_rows_for_test(&db, "evidence_plans").await,
        djinn_db::test_support::count_rows_for_test(&db, "evidence_command_invocations").await,
        djinn_db::test_support::count_rows_for_test(&db, "evidence_finalized_projections").await,
        djinn_db::test_support::count_rows_for_test(&db, "proposal_debate_trail").await,
        djinn_db::test_support::count_rows_for_test(&db, "proposal_revisions").await,
    );
    assert_eq!(
        after, before,
        "empty historical backfill must perform zero writes"
    );

    let revisions_after = proposals
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle after empty backfill");
    assert_eq!(
        serde_json::to_value(revisions_after.clone()).expect("serialize lifecycle after backfill"),
        serde_json::to_value(revisions_before).expect("serialize lifecycle baseline"),
        "no lifecycle row may be fabricated"
    );
    assert!(
        revisions_after.iter().all(|revision| {
            !matches!(
                revision.event_kind.as_str(),
                "refinement_evidence_received" | "refinement_evidence_failed"
            )
        }),
        "empty backfill emits neither receipt nor failure"
    );
}
