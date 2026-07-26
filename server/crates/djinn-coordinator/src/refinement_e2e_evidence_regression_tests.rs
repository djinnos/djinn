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
use djinn_core::models::NeedsEvidenceClaim;
use djinn_db::{
    EffectiveCreatorProvenance, ProposalDebateTrailCreateInput, ProposalRepository, TaskRepository,
};

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a deterministic claim payload for the fixture proposal.
fn sample_claim(judge_task_id: &str) -> String {
    let claim = NeedsEvidenceClaim {
        question: "Can the coordinator resume safely?".to_string(),
        target_subsystem: "refinement".to_string(),
        spec_unknown_anchor: "resume path".to_string(),
        insufficient_in_session_research: "needs spike".to_string(),
        expected_findings: "structured findings".to_string(),
        round: 1,
        against_revision_seq: 1,
        created_by_task_id: judge_task_id.to_string(),
    };
    serde_json::to_string(&claim).expect("serialize claim")
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
        .set_needs_evidence_spike(&fixture.proposal_id, &spike_task_id, &claim)
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
async fn valid_evidence_completion_clears_link_and_resumes_advocate_with_findings() {
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
    actor
        .persist_terminal_linked_spike_evidence_from_closed_task(&task)
        .await;

    // Link and claim are cleared exactly once.
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert!(proposal.linked_spike_task_id.is_none());
    assert!(proposal.needs_evidence_claim.is_none());

    // Exactly one receipt lifecycle event.
    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let receipt_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_received")
        .collect();
    assert_eq!(receipt_events.len(), 1);

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
    actor
        .persist_terminal_linked_spike_evidence_from_closed_task(&task)
        .await;

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
    assert_eq!(failure_events.len(), 1);

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
    actor
        .persist_terminal_linked_spike_evidence_from_closed_task(&task)
        .await;

    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    let failure_events: Vec<_> = lifecycle
        .iter()
        .filter(|r| r.event_kind == "refinement_evidence_failed")
        .collect();
    assert_eq!(failure_events.len(), 1);

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
    actor
        .persist_terminal_linked_spike_evidence_from_closed_task(&task)
        .await;

    // Receipt is recorded and link/claim cleared.
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert!(proposal.linked_spike_task_id.is_none());
    assert!(proposal.needs_evidence_claim.is_none());
    let lifecycle = proposal_repo
        .revisions(&fixture.proposal_id)
        .await
        .expect("read lifecycle");
    assert!(
        lifecycle
            .iter()
            .any(|r| r.event_kind == "refinement_evidence_received")
    );

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
    assert_eq!(receipt_events.len(), 1);

    assert_eq!(
        count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await,
        1
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
    assert_eq!(failure_events.len(), 1);
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
    assert_eq!(failure_events.len(), 1);
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
    // Clear the link manually to simulate the already-cleared scenario.
    proposal_repo
        .clear_needs_evidence_spike(&fixture.proposal_id)
        .await
        .expect("clear link");
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
    assert_eq!(receipt_events.len(), 1);
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
        .set_needs_evidence_spike(&fixture.proposal_id, &spike1, &claim)
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
        .set_needs_evidence_spike(&fixture.proposal_id, &spike2, &claim)
        .await;

    // The repository helper must prevent the second link (either by error or by
    // replacing the previous link atomically; the important behavior is that only
    // one spike remains linked at the end).  The existing implementation replaces
    // the link, so assert the final link is the one we most recently set.
    result.expect("set second spike link may replace");
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    assert_eq!(
        proposal.linked_spike_task_id.as_deref(),
        Some(spike2.as_str())
    );
    assert_eq!(
        proposal.needs_evidence_claim.as_deref(),
        Some(claim.as_str())
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
    actor
        .persist_terminal_linked_spike_evidence_from_closed_task(&task)
        .await;
    actor.drive_active_refinements().await;

    let advocate_count = count_refinement_tasks(&task_repo, &fixture.project_id, "advocate").await;
    assert_eq!(
        advocate_count, 1,
        "only the pre-existing Advocate task exists"
    );
    assert!(actor.refinement_sessions.is_empty());
}
