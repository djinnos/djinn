// djinn:allow-oversize — repository-backed outcome and coordinator-fault invariants share exact-run fixtures.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovered_advocate_projection_scores_revisions_after_snapshot_as_advanced() {
    let mut f = durable_outcome_fixture().await;
    let repo = ProposalRepository::new(f.db.clone(), djinn_core::events::EventBus::noop());
    let captured = repo
        .refinement_run_captured_snapshot_seq(&f.run_id)
        .await
        .unwrap();
    let original = repo.get(&f.fixture.proposal_id).await.unwrap().unwrap();
    repo.update(
        &f.fixture.proposal_id,
        djinn_db::repositories::proposal::ProposalUpdateInput {
            title: &original.title,
            body: "advocate advanced body",
            acceptance_criteria: "[]",
            status: &original.status,
            superseded_by: None,
            body_format: Some(&original.body_format),
            event_metadata: None,
        },
    )
    .await
    .unwrap();
    let mut rebuilt = RefinementLoopState::new(&f.fixture.proposal_id, 2)
        .with_run_identity(f.run_id.clone(), f.generation)
        .with_recovered_snapshot_seq(captured);
    rebuilt.phase = RefinementPhase::AdvocateRevision;
    rebuilt.current_round = 1;
    let candidate = f
        .actor
        .process_advocate_outcome(&f.run_id, &f.fixture.proposal_id, &f.task_id, &rebuilt)
        .await
        .expect("productive Advocate result is applicable");
    assert_eq!(candidate.current_revision_seq, 2);
    assert_eq!(candidate.phase, RefinementPhase::JudgeAdjudication);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_outcome_attempt_cap_survives_projection_rebuild() {
    let mut f = durable_outcome_fixture().await;
    djinn_db::test_support::close_task_at(&f.db, &f.task_id, "2026-08-01T00:00:01.000Z").await;
    let repo = ProposalRepository::new(f.db.clone(), djinn_core::events::EventBus::noop());
    assert_eq!(
        repo.increment_refinement_outcome_attempt(&f.run_id, f.generation, &f.task_id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        repo.increment_refinement_outcome_attempt(&f.run_id, f.generation, &f.task_id)
            .await
            .unwrap(),
        2
    );
    f.actor.active_refinements.clear();
    f.actor.refinement_sessions.clear();
    f.actor.recover_interrupted_refinements().await;
    assert!(f.actor.active_refinements.contains_key(&f.run_id));
    f.actor
        .handle_stalled_outcome_application(
            &f.run_id,
            &f.session,
            RefinementOutcomeApplication::Retryable,
        )
        .await;
    let exact = repo
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: f.run_id.clone(),
            heartbeat_grace_millis: 60_000,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exact.snapshot.run.state, RefinementRunState::Terminal);
    assert!(exact.snapshot.run.terminal_reason.is_some());
}

// ---- is_already_closed_refinement_close_error ----

#[test]
fn force_close_already_closed_returns_true() {
    let error = djinn_db::Error::InvalidTransition("task is already closed".to_owned());
    assert!(is_already_closed_refinement_close_error(&error));
}

#[test]
fn force_close_other_invalid_transition_returns_false() {
    let error =
        djinn_db::Error::InvalidTransition("release is only valid from in_progress".to_owned());
    assert!(!is_already_closed_refinement_close_error(&error));
}

#[test]
fn force_close_non_transition_error_returns_false() {
    let error = djinn_db::Error::Internal("something broke".to_owned());
    assert!(!is_already_closed_refinement_close_error(&error));
}

// ---- handle_close_refinement_task_result regression tests ----

#[test]
#[tracing_test::traced_test]
fn close_already_closed_emits_no_warning() {
    let already_closed = djinn_db::Error::InvalidTransition("task is already closed".to_owned());
    handle_close_refinement_task_result("task/abc", Err(already_closed));

    assert!(
        !logs_contain("Failed to close completed refinement task"),
        "already-closed close should not emit a warning"
    );
}

#[test]
#[tracing_test::traced_test]
fn close_other_invalid_transition_emits_warning() {
    let other =
        djinn_db::Error::InvalidTransition("release is only valid from in_progress".to_owned());
    handle_close_refinement_task_result("task/xyz", Err(other));

    assert!(
        logs_contain("Failed to close completed refinement task"),
        "non-idempotent InvalidTransition must still warn"
    );
}

#[test]
#[tracing_test::traced_test]
fn close_internal_error_emits_warning() {
    let internal = djinn_db::Error::Internal("database connection lost".to_owned());
    handle_close_refinement_task_result("task/123", Err(internal));

    assert!(
        logs_contain("Failed to close completed refinement task"),
        "internal/repository errors must still warn"
    );
}

// `logs_contain` is injected by the `#[tracing_test::traced_test]` macro
// into each test function scope; no module-level helper is needed.

// ---- current-run debate-trail scoping (cross-run collision) ----

/// Build a judge verdict debate-trail entry with an explicit `created_at`
/// so tests can reproduce a trail that spans two refinement runs.
fn verdict_entry(
    round: i32,
    against_revision_seq: i32,
    blocking: bool,
    created_at: &str,
) -> ProposalDebateTrail {
    ProposalDebateTrail {
        id: format!("verdict/{created_at}"),
        proposal_id: "p1".into(),
        kind: "verdict".into(),
        body: if blocking { "needs work" } else { "approve" }.into(),
        blocking,
        agent_role: "judge".into(),
        author_kind: "agent".into(),
        author_user_id: None,
        author_model: None,
        source_task_id: None,
        against_revision_seq,
        round,
        body_metadata: None,
        resolved_at: None,
        resolved_by_user_id: None,
        reopened_at: None,
        reopened_by_user_id: None,
        created_at: created_at.into(),
        updated_at: created_at.into(),
    }
}

/// Incident 019f0c29: run #1 produced a round-1 APPROVE verdict (against
/// revision seq 2), was interrupted by a restart, then run #2 produced a
/// round-1 NEEDS-WORK verdict (against revision seq 3). The debate trail is
/// ordered `round, created_at`, so a naive `.find()` returned the stale
/// approve. With current-run scoping the fresh needs-work verdict must win.
#[test]
fn verdict_scoping_ignores_stale_prior_run_approve() {
    // Trail ordered as `debate_trail()` returns it (round, then created_at).
    let entries = vec![
        // Run #1, round 1: stale approve (interrupted run).
        verdict_entry(1, 2, false, "2026-07-08T10:00:00.000Z"),
        // Run #2, round 1: fresh needs-work.
        verdict_entry(1, 3, true, "2026-07-08T10:00:40.000Z"),
    ];
    // Run #2 started between the two verdicts.
    let run_start = Some("2026-07-08T10:00:30.000Z");

    let selected = select_current_run_verdict(&entries, 1, 3, run_start)
        .expect("a current-run verdict must be selected");
    assert!(
        selected.blocking,
        "must select the fresh needs-work verdict, not the stale approve"
    );
    assert_eq!(selected.against_revision_seq, 3);

    // The state machine must run another round, not park for human review.
    let mut state = RefinementLoopState::with_config("p1", 3, test_config());
    state.record_judge_verdict(&JudgeVerdictResult {
        body: selected.body.clone(),
        blocking: selected.blocking,
    });
    assert_eq!(state.phase, RefinementPhase::AdversaryAttack);
    assert!(!state.is_awaiting_human_review());
    // …and the round it re-opens carries the outstanding remedy, so a dry
    // Adversary hands the round to the Advocate rather than stranding it.
    assert!(state.pending_blocking_verdict);
}

/// Belt-and-braces: even with no `refinement_start` boundary recorded
/// (`run_start == None`), the `against_revision_seq == current_revision_seq`
/// preference plus latest-by-`created_at` tie-break still selects the fresh
/// verdict rather than the stale approve.
#[test]
fn verdict_selection_prefers_current_revision_without_boundary() {
    let entries = vec![
        verdict_entry(1, 2, false, "2026-07-08T10:00:00.000Z"),
        verdict_entry(1, 3, true, "2026-07-08T10:00:40.000Z"),
    ];
    let selected =
        select_current_run_verdict(&entries, 1, 3, None).expect("a verdict must be selected");
    assert!(
        selected.blocking,
        "must prefer the current-revision verdict"
    );
    assert_eq!(selected.against_revision_seq, 3);
}

/// When several verdicts match the current revision (e.g. a re-run wrote a
/// second one), the LATEST by `created_at` wins — never the oldest.
#[test]
fn verdict_selection_takes_latest_on_tie() {
    let entries = vec![
        verdict_entry(1, 3, false, "2026-07-08T10:00:40.000Z"),
        verdict_entry(1, 3, true, "2026-07-08T10:01:10.000Z"),
    ];
    let selected = select_current_run_verdict(&entries, 1, 3, Some("2026-07-08T10:00:30.000Z"))
        .expect("a verdict must be selected");
    assert!(selected.blocking, "latest verdict must win the tie");
    assert_eq!(selected.created_at, "2026-07-08T10:01:10.000Z");
}

#[test]
fn entry_in_current_run_boundary_semantics() {
    let entry = verdict_entry(1, 1, false, "2026-07-08T10:00:30.000Z");
    // Strictly after the boundary → in-run.
    assert!(entry_in_current_run(
        &entry,
        Some("2026-07-08T10:00:00.000Z")
    ));
    // At or before the boundary → prior run.
    assert!(!entry_in_current_run(
        &entry,
        Some("2026-07-08T10:00:30.000Z")
    ));
    assert!(!entry_in_current_run(
        &entry,
        Some("2026-07-08T10:01:00.000Z")
    ));
    // No boundary → always in-run.
    assert!(entry_in_current_run(&entry, None));
}

fn test_config() -> super::super::refinement::RefinementConfig {
    super::super::refinement::RefinementConfig::default()
}

// ---- Advocate structured lint retry ----

#[test]
fn structured_lint_rejection_preserves_order_for_correction_prompt() {
    let payload = r#"{"tool_result":{"code":"SPEC_LINT_REJECTED","violations":[{"code":"SECOND","message":"second message","span":{"start_byte":20,"end_byte":24}},{"code":"FIRST","message":"first message","span":{"start_byte":4,"end_byte":9}}]}}"#;
    let violations = parse_spec_lint_rejection(payload).expect("structured lint rejection");
    assert_eq!(violations.len(), 2);
    assert_eq!(
        violations[0].code, "SECOND",
        "do not reorder authoring diagnostics"
    );
    assert_eq!(violations[1].code, "FIRST");
    let context = format_advocate_lint_correction_context(&violations).expect("correction context");
    assert!(context.contains("SECOND: second message at bytes 20..24"));
    assert!(context.find("SECOND").unwrap() < context.find("FIRST").unwrap());
}

#[test]
fn persisted_tool_result_evidence_drives_lint_retry_not_assistant_prose() {
    use djinn_core::message::{ContentBlock, Conversation, Message, Role};

    let payload = r#"{"code":"SPEC_LINT_REJECTED","violations":[{"code":"SECOND","message":"second message","span":{"start_byte":20,"end_byte":24}},{"code":"FIRST","message":"first message","span":{"start_byte":4,"end_byte":9}}]}"#;
    let mut conversation = Conversation::default();
    // This mirrors reply_loop/turn.rs: ToolResult is stored in a user message.
    conversation.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: format!("The tool said: {payload}"),
        }],
        metadata: None,
    });
    conversation.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "proposal_update_1".into(),
            content: vec![ContentBlock::Text {
                text: payload.into(),
            }],
            is_error: true,
        }],
        metadata: None,
    });

    let violations = parse_spec_lint_rejection_from_conversation(&conversation)
        .expect("structured rejection in persisted ToolResult");
    assert_eq!(violations[0].code, "SECOND");
    assert_eq!(violations[1].code, "FIRST");

    let mut prose_only = Conversation::default();
    prose_only.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: payload.into(),
        }],
        metadata: None,
    });
    assert!(parse_spec_lint_rejection_from_conversation(&prose_only).is_none());
}

#[test]
fn ordinary_no_change_payload_is_not_a_lint_rejection() {
    assert!(parse_spec_lint_rejection(r#"{"ok":true,"message":"no revision"}"#).is_none());
    assert!(parse_spec_lint_rejection(r#"{"code":"SPEC_LINT_REJECTED"}"#).is_none());
}

/// A completed pass may retain an earlier rejected ToolResult even after a
/// later clean write. The coordinator must consult that evidence only when the
/// material head did not advance; otherwise the clean revision proceeds to the
/// Judge rather than causing a redundant same-round retry.
#[test]
fn clean_revision_takes_precedence_over_earlier_lint_rejection_evidence() {
    let source = include_str!("refinement_outcome.rs");
    let revision_check = source
        .find("let advanced = new_revision_seq > state.current_revision_seq;")
        .expect("advocate outcome must determine whether the head advanced");
    let lint_check = source
        .find("if !advanced {")
        .expect("lint evidence must be conditional on an unchanged head");
    assert!(
        revision_check < lint_check,
        "a clean revision must take precedence over historical rejection evidence"
    );
    let evidence_source = include_str!("refinement_lint_evidence.rs");
    assert!(
        evidence_source.contains(".load_raw_conversation(&session.id)"),
        "lint classification must inspect uncompacted persisted ToolResult evidence"
    );
}

#[test]
fn lint_rejection_keeps_advocate_in_same_round_and_revision() {
    let mut state = RefinementLoopState::with_config("p1", 7, test_config());
    state.phase = RefinementPhase::AdvocateRevision;
    state.current_round = 3;
    state.record_advocate_lint_rejection(vec![super::super::refinement::AdvocateLintViolation {
        code: "DUPLICATE_BLOCK_ID".into(),
        message: "duplicate id".into(),
        start_byte: 12,
        end_byte: 24,
    }]);
    assert_eq!(state.phase, RefinementPhase::AdvocateRevision);
    assert_eq!(
        state.current_round, 3,
        "failed candidate must not consume a round"
    );
    assert_eq!(
        state.current_revision_seq, 7,
        "failed candidate must not become a revision"
    );
    assert_eq!(state.pending_advocate_lint_violations.len(), 1);
    state.record_advocate_revision(8);
    assert_eq!(state.phase, RefinementPhase::JudgeAdjudication);
    assert_eq!(state.current_revision_seq, 8);
    assert!(state.pending_advocate_lint_violations.is_empty());
}

#[test]
fn repeated_lint_rejections_are_bounded_by_existing_spawn_cap() {
    let mut config = test_config();
    config.max_total_spawns = 2;
    let mut state = RefinementLoopState::with_config("p1", 7, config);
    state.phase = RefinementPhase::AdvocateRevision;
    for _ in 0..2 {
        state
            .record_spawn()
            .expect("existing cap admits correction session");
        state.record_advocate_lint_rejection(vec![
            super::super::refinement::AdvocateLintViolation {
                code: "DUPLICATE_BLOCK_ID".into(),
                message: "duplicate id".into(),
                start_byte: 0,
                end_byte: 1,
            },
        ]);
        assert_eq!(state.current_round, 1);
        assert_eq!(state.current_revision_seq, 7);
        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);
    }
    assert!(matches!(
        state.record_spawn(),
        Err(super::super::refinement::StopReason::AgentFailure { role: djinn_core::refinement_liveness::RefinementRole::Advocate, ref message, .. })
            if message.contains("SPEC_LINT_REJECTED")
    ));
    assert!(
        state.is_complete(),
        "persistent rejections terminate at the established cap"
    );
    assert_eq!(
        state.current_round, 1,
        "failed writes never consume a refinement round"
    );
}

#[test]
fn outcome_application_distinguishes_retry_from_commit() {
    assert_ne!(
        RefinementOutcomeApplication::Retryable,
        RefinementOutcomeApplication::Committed
    );
    assert_ne!(
        RefinementOutcomeApplication::Ignored,
        RefinementOutcomeApplication::Committed
    );
}

use std::time::Instant;

use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    models::TaskRefinementCorrelation,
    refinement_liveness::{
        RefinementIntentState, RefinementPhase as DurablePhase, RefinementRole, RefinementRunState,
    },
};
use djinn_db::{
    AcknowledgeRefinementTaskMaterializationRequest, AdmitRefinementRunRequest,
    ClaimRefinementIntentRequest, CompleteRefinementIntentRequest,
    LoadRefinementRunSnapshotRequest, ProposalCreateInput, ProposalDebateTrailCreateInput,
    RefinementAdmissionOutcome, RefinementAdmissionSource, UserRepository,
};

use crate::refinement_dispatch::refinement_cap_tests::{build_refinement_actor, spawn_test_pool};

struct OutcomeProposalFixture {
    proposal_id: String,
    user_id: String,
}

struct DurableOutcomeFixture {
    db: djinn_db::Database,
    actor: CoordinatorActor,
    fixture: OutcomeProposalFixture,
    run_id: String,
    generation: i32,
    intent_id: String,
    task_id: String,
    session: RefinementSession,
    projection: RefinementLoopState,
}

#[allow(clippy::too_many_arguments)]
async fn materialize_outcome_intent(
    actor: &CoordinatorActor,
    fixture: &OutcomeProposalFixture,
    run_id: &str,
    generation: i32,
    intent_id: &str,
    round: i32,
    phase: DurablePhase,
    role: RefinementRole,
) -> (String, RefinementSession) {
    let repo = ProposalRepository::new(actor.db.clone(), EventBus::noop());
    let owner = format!("coordinator:{}", actor.coordinator_incarnation_id);
    repo.claim_refinement_intent(ClaimRefinementIntentRequest {
        run_id: run_id.into(),
        intent_id: intent_id.into(),
        generation,
        owner: owner.clone(),
        lease_millis: 60_000,
    })
    .await
    .expect("claim source intent")
    .expect("source lease acquired");
    let correlation = TaskRefinementCorrelation::new(
        run_id.into(),
        intent_id.into(),
        i64::from(generation),
        i64::from(round),
        phase,
        role,
    )
    .expect("valid source correlation");
    let (local_phase, role_name) = match phase {
        DurablePhase::AdversaryAttack => (RefinementPhase::AdversaryAttack, "adversary"),
        DurablePhase::AdvocateRevision => (RefinementPhase::AdvocateRevision, "advocate"),
        DurablePhase::JudgeAdjudication => (RefinementPhase::JudgeAdjudication, "judge"),
    };
    let head = repo
        .get(&fixture.proposal_id)
        .await
        .expect("load proposal")
        .expect("proposal exists")
        .latest_revision_seq;
    let task_id = actor
        .create_refinement_task_with_context_and_correlation(
            &fixture.proposal_id,
            role_name,
            round,
            head,
            "durable outcome regression",
            None,
            Some(&fixture.user_id),
            Some(&correlation),
        )
        .await
        .expect("create correlated source task");
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
        .expect("acknowledge source task")
    );
    (
        task_id.clone(),
        RefinementSession {
            run_id: run_id.into(),
            generation,
            task_id,
            phase: local_phase,
            dispatched_at: Instant::now(),
            session_started_at: Some(Instant::now()),
            model_id: "test/mock".into(),
        },
    )
}

async fn seed_outcome_proposal(db: &djinn_db::Database) -> OutcomeProposalFixture {
    let project = crate::test_helpers::create_test_project(db).await;
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            777_101,
            "refinement-outcome-user",
            Some("Refinement outcome test user"),
            None,
        )
        .await
        .expect("create outcome test user");
    let proposal = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user.id.clone()), async {
            ProposalRepository::new(db.clone(), EventBus::noop())
                .create(ProposalCreateInput {
                    title: "Durable outcome test proposal",
                    body: "A proposal for correlated durable outcome tests.",
                    acceptance_criteria: Some("[]"),
                    status: Some("building"),
                    body_format: None,
                })
                .await
                .expect("create outcome test proposal")
        })
        .await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    repo.add_target(&proposal.id, &project.id, "primary")
        .await
        .expect("add outcome proposal target");
    repo.start_refinement_with_owner(&proposal.id, Some(&user.id))
        .await
        .expect("persist outcome refinement owner");
    OutcomeProposalFixture {
        proposal_id: proposal.id,
        user_id: user.id,
    }
}

async fn durable_outcome_fixture() -> DurableOutcomeFixture {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_outcome_proposal(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 1);
    let actor = build_refinement_actor(&db, &events_tx, pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let admitted = repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id.clone(),
            idempotency_key: uuid::Uuid::now_v7().to_string(),
            source: RefinementAdmissionSource::Demand {
                demand_id: uuid::Uuid::now_v7().to_string(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit durable outcome run");
    let (run_id, intent_id, generation) = match admitted {
        RefinementAdmissionOutcome::Admitted {
            run_id,
            intent_id,
            generation,
        }
        | RefinementAdmissionOutcome::Existing {
            run_id,
            intent_id,
            generation,
        } => (run_id, intent_id, generation),
    };
    let head = repo
        .get(&fixture.proposal_id)
        .await
        .expect("load proposal")
        .expect("proposal exists")
        .latest_revision_seq;
    let projection = RefinementLoopState::new(&fixture.proposal_id, head)
        .with_run_identity(run_id.clone(), generation)
        .with_attributed_user(Some(fixture.user_id.clone()));
    let (task_id, session) = materialize_outcome_intent(
        &actor,
        &fixture,
        &run_id,
        generation,
        &intent_id,
        1,
        DurablePhase::AdversaryAttack,
        RefinementRole::Adversary,
    )
    .await;
    DurableOutcomeFixture {
        db,
        actor,
        fixture,
        run_id,
        generation,
        intent_id,
        task_id,
        session,
        projection,
    }
}

async fn admit_foreign_outcome_run(db: &djinn_db::Database) -> (String, String) {
    let fixture = seed_outcome_proposal(db).await;
    let admitted = ProposalRepository::new(db.clone(), EventBus::noop())
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id,
            idempotency_key: uuid::Uuid::now_v7().to_string(),
            source: RefinementAdmissionSource::Demand {
                demand_id: uuid::Uuid::now_v7().to_string(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit durable foreign run");
    match admitted {
        RefinementAdmissionOutcome::Admitted {
            run_id, intent_id, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, intent_id, ..
        } => (run_id, intent_id),
    }
}

async fn snapshot(f: &DurableOutcomeFixture) -> djinn_db::RefinementRunSnapshotResult {
    ProposalRepository::new(f.db.clone(), EventBus::noop())
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: f.run_id.clone(),
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("load exact run snapshot")
        .expect("run exists")
}

fn reset_rejected_outcome_counters(f: &DurableOutcomeFixture, task_id: &str) {
    reset_outcome_test_seam(task_id);
    reset_outcome_test_seam(&f.fixture.proposal_id);
}

fn assert_rejected_outcome_skipped_reads(f: &DurableOutcomeFixture) {
    let counters = outcome_test_seam_counters(&f.fixture.proposal_id);
    assert_eq!(
        counters.proposal_reads, 0,
        "rejected outcome must not read the proposal"
    );
    assert_eq!(
        counters.debate_reads, 0,
        "rejected outcome must not read the debate trail"
    );
    assert_eq!(
        counters.progress_writes, 0,
        "rejected outcome must not write durable progress"
    );
}

fn assert_exact_durable_run_and_intents(
    actual: &djinn_db::RefinementRunSnapshotResult,
    expected: &djinn_db::RefinementRunSnapshotResult,
    context: &str,
) {
    assert_eq!(
        actual.proposal_id, expected.proposal_id,
        "{context}: proposal id"
    );
    assert_eq!(
        actual.generation, expected.generation,
        "{context}: generation"
    );
    assert_eq!(
        actual.snapshot, expected.snapshot,
        "{context}: complete durable run and intent snapshot"
    );
}

/// A rejected outcome must stop at the durable correlation fence. In
/// particular, neither the durable source nor either disposable projection may
/// move before a repository transition commits.
async fn assert_rejected_outcome_preserves_source(
    f: &mut DurableOutcomeFixture,
    session: RefinementSession,
    injected_failure: Option<OutcomeTestSeamPoint>,
) {
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), f.projection.clone());
    f.actor
        .refinement_sessions
        .insert(f.run_id.clone(), f.session.clone());
    let durable_before = snapshot(f).await;
    let lifecycle_before = ProposalRepository::new(f.db.clone(), EventBus::noop())
        .revisions(&f.fixture.proposal_id)
        .await
        .expect("read lifecycle before rejected outcome")
        .into_iter()
        .filter(|revision| revision.event_kind == "refinement_stop")
        .count();
    let task_before = TaskRepository::new(f.db.clone(), EventBus::noop())
        .get(&f.task_id)
        .await
        .expect("load exact source task before rejected outcome");
    let projection_before = f.actor.active_refinements[&f.run_id].clone();
    let session_before = f.actor.refinement_sessions[&f.run_id].clone();
    reset_rejected_outcome_counters(f, &session.task_id);
    if let Some(point) = injected_failure {
        inject_outcome_test_failure(&session.task_id, point);
    }

    let expected_application = if injected_failure.is_some() {
        RefinementOutcomeApplication::Retryable
    } else {
        RefinementOutcomeApplication::Ignored
    };
    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &session)
            .await,
        expected_application
    );
    assert_exact_durable_run_and_intents(
        &snapshot(f).await,
        &durable_before,
        "rejected outcome must not move",
    );
    assert_eq!(
        format!(
            "{:#?}",
            TaskRepository::new(f.db.clone(), EventBus::noop())
                .get(&f.task_id)
                .await
                .expect("load exact source task after rejected outcome")
        ),
        format!("{task_before:#?}"),
        "rejected outcome must not mutate the durable task row"
    );
    assert_eq!(
        ProposalRepository::new(f.db.clone(), EventBus::noop())
            .revisions(&f.fixture.proposal_id)
            .await
            .expect("read lifecycle after rejected outcome")
            .into_iter()
            .filter(|revision| revision.event_kind == "refinement_stop")
            .count(),
        lifecycle_before,
        "rejected outcome must not append a proposal-scoped stop"
    );
    assert_eq!(
        format!("{:#?}", f.actor.active_refinements[&f.run_id]),
        format!("{projection_before:#?}"),
        "rejected outcome must not publish a projection"
    );
    assert_eq!(
        format!("{:#?}", f.actor.refinement_sessions[&f.run_id]),
        format!("{session_before:#?}"),
        "rejected outcome must retain the complete original session"
    );
    assert_rejected_outcome_skipped_reads(f);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attributed_user_repository_lookup_failure_creates_no_stop_or_projection_mutation() {
    let mut f = durable_outcome_fixture().await;
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), f.projection.clone());
    let before = snapshot(&f).await;
    let lifecycle_before = ProposalRepository::new(f.db.clone(), EventBus::noop())
        .revisions(&f.fixture.proposal_id)
        .await
        .expect("read lifecycle before attributed-user lookup failure")
        .into_iter()
        .filter(|revision| revision.event_kind == "refinement_stop")
        .count();
    let projection_before = format!("{:#?}", f.actor.active_refinements[&f.run_id]);

    assert!(
        f.actor
            .resolve_owner_identity("missing-attributed-user")
            .await
            .is_err()
    );

    assert_exact_durable_run_and_intents(
        &snapshot(&f).await,
        &before,
        "attributed-user repository lookup failure",
    );
    assert_eq!(
        ProposalRepository::new(f.db.clone(), EventBus::noop())
            .revisions(&f.fixture.proposal_id)
            .await
            .expect("read lifecycle after attributed-user lookup failure")
            .into_iter()
            .filter(|revision| revision.event_kind == "refinement_stop")
            .count(),
        lifecycle_before
    );
    assert_eq!(
        format!("{:#?}", f.actor.active_refinements[&f.run_id]),
        projection_before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlated_task_creation_request_failure_creates_no_stop_or_projection_mutation() {
    let mut f = durable_outcome_fixture().await;
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), f.projection.clone());
    let before = snapshot(&f).await;
    let lifecycle_before = ProposalRepository::new(f.db.clone(), EventBus::noop())
        .revisions(&f.fixture.proposal_id)
        .await
        .expect("read lifecycle before correlated task request failure")
        .into_iter()
        .filter(|revision| revision.event_kind == "refinement_stop")
        .count();
    let projection_before = format!("{:#?}", f.actor.active_refinements[&f.run_id]);
    let correlation = TaskRefinementCorrelation::new(
        f.run_id.clone(),
        f.intent_id.clone(),
        i64::from(f.generation),
        1,
        DurablePhase::AdversaryAttack,
        RefinementRole::Adversary,
    )
    .expect("valid duplicate exact correlation");

    assert!(
        f.actor
            .create_refinement_task_with_context_and_correlation(
                &f.fixture.proposal_id,
                "adversary",
                1,
                f.projection.current_revision_seq,
                "duplicate correlated request must fail",
                None,
                Some(&f.fixture.user_id),
                Some(&correlation),
            )
            .await
            .is_none(),
        "repository uniqueness must reject a second task for one intent"
    );

    assert_exact_durable_run_and_intents(
        &snapshot(&f).await,
        &before,
        "correlated task creation request failure",
    );
    assert_eq!(
        ProposalRepository::new(f.db.clone(), EventBus::noop())
            .revisions(&f.fixture.proposal_id)
            .await
            .expect("read lifecycle after correlated task request failure")
            .into_iter()
            .filter(|revision| revision.event_kind == "refinement_stop")
            .count(),
        lifecycle_before
    );
    assert_eq!(
        format!("{:#?}", f.actor.active_refinements[&f.run_id]),
        projection_before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outcome_handler_task_reload_failure_creates_no_stop_or_projection_mutation() {
    let mut f = durable_outcome_fixture().await;
    let session = f.session.clone();
    assert_rejected_outcome_preserves_source(
        &mut f,
        session,
        Some(OutcomeTestSeamPoint::TaskReload),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlation_fence_rejects_missing_and_mismatched_task_identity() {
    // Each row uses a repository-backed task row. Replacing its correlation
    // verifies every independently-corrupted identity component retains the
    // exact durable source and both disposable projections.
    let cases = vec![
        ("missing intent correlation", None),
        (
            "stale generation",
            Some((
                "same-run".to_owned(),
                "same-intent".to_owned(),
                2,
                1,
                DurablePhase::AdversaryAttack,
                RefinementRole::Adversary,
            )),
        ),
        (
            "wrong round",
            Some((
                "same-run".to_owned(),
                "same-intent".to_owned(),
                1,
                2,
                DurablePhase::AdversaryAttack,
                RefinementRole::Adversary,
            )),
        ),
        (
            "stale phase",
            Some((
                "same-run".to_owned(),
                "same-intent".to_owned(),
                1,
                1,
                DurablePhase::AdvocateRevision,
                RefinementRole::Advocate,
            )),
        ),
        // A raw durable row can contain an invalid phase/role pairing even though
        // the typed constructor prevents new writers from creating one.
        ("wrong role", None),
        (
            "foreign run",
            Some((
                "foreign-run".to_owned(),
                "same-intent".to_owned(),
                1,
                1,
                DurablePhase::AdversaryAttack,
                RefinementRole::Adversary,
            )),
        ),
        (
            "non-current source intent",
            Some((
                "same-run".to_owned(),
                "foreign-intent".to_owned(),
                1,
                1,
                DurablePhase::AdversaryAttack,
                RefinementRole::Adversary,
            )),
        ),
    ];

    for (name, replacement) in cases {
        let mut f = durable_outcome_fixture().await;
        let foreign_evidence = if matches!(name, "foreign run" | "non-current source intent") {
            Some(admit_foreign_outcome_run(&f.db).await)
        } else {
            None
        };
        let correlation = replacement.map(|(run, intent, generation, round, phase, role)| {
            TaskRefinementCorrelation::new(
                if run == "same-run" {
                    f.run_id.clone()
                } else if run == "foreign-run" {
                    foreign_evidence
                        .as_ref()
                        .expect("foreign run case must materialize durable foreign evidence")
                        .0
                        .clone()
                } else {
                    run
                },
                if intent == "same-intent" {
                    f.intent_id.clone()
                } else if intent == "foreign-intent" {
                    foreign_evidence
                        .as_ref()
                        .expect("non-current intent case must materialize durable foreign evidence")
                        .1
                        .clone()
                } else {
                    intent
                },
                i64::from(if generation == 1 {
                    f.generation
                } else {
                    generation
                }),
                round,
                phase,
                role,
            )
            .expect("valid deliberately mismatched correlation")
        });
        if name == "wrong role" {
            djinn_db::test_support::corrupt_refinement_task_role_for_test(
                &f.db, &f.task_id, "advocate",
            )
            .await;
        } else {
            TaskRepository::new(f.db.clone(), EventBus::noop())
                .set_refinement_correlation(&f.task_id, correlation.as_ref())
                .await
                .expect("replace task correlation for fence case");
        }
        let session = f.session.clone();
        assert_rejected_outcome_preserves_source(&mut f, session, None).await;
        assert_eq!(
            snapshot(&f).await.snapshot.intents[0].state,
            RefinementIntentState::Materialized,
            "{name} must retain the materialized source intent"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlation_fence_rejects_session_generation_and_phase_mismatch() {
    let mut f = durable_outcome_fixture().await;

    // Session identity checks occur even earlier than the task-row fence.
    let mut wrong_generation = f.session.clone();
    wrong_generation.generation += 1;
    assert_rejected_outcome_preserves_source(&mut f, wrong_generation, None).await;
    let mut wrong_phase = f.session.clone();
    wrong_phase.phase = RefinementPhase::AdvocateRevision;
    assert_rejected_outcome_preserves_source(&mut f, wrong_phase, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_correlated_task_is_retryable_without_moving_any_projection() {
    let mut f = durable_outcome_fixture().await;
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), f.projection.clone());
    f.actor
        .refinement_sessions
        .insert(f.run_id.clone(), f.session.clone());
    let durable_before = snapshot(&f).await;
    let task_before = TaskRepository::new(f.db.clone(), EventBus::noop())
        .get(&f.task_id)
        .await
        .expect("load exact durable source task before missing-task outcome");
    let projection_before = f.actor.active_refinements[&f.run_id].clone();
    let session_before = f.actor.refinement_sessions[&f.run_id].clone();
    let mut missing_task = f.session.clone();
    missing_task.task_id = uuid::Uuid::now_v7().to_string();
    assert!(
        TaskRepository::new(f.db.clone(), EventBus::noop())
            .get(&missing_task.task_id)
            .await
            .expect("confirm missing correlated task is absent before outcome")
            .is_none(),
        "missing task case requires no task row"
    );
    reset_rejected_outcome_counters(&f, &missing_task.task_id);

    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &missing_task)
            .await,
        RefinementOutcomeApplication::Retryable
    );
    assert_exact_durable_run_and_intents(
        &snapshot(&f).await,
        &durable_before,
        "missing task must preserve exact durable run and intents",
    );
    assert_eq!(
        format!(
            "{:#?}",
            TaskRepository::new(f.db.clone(), EventBus::noop())
                .get(&f.task_id)
                .await
                .expect("reload exact durable source task after missing-task outcome")
        ),
        format!("{task_before:#?}"),
        "missing task outcome must preserve the exact source task row"
    );
    assert!(
        TaskRepository::new(f.db.clone(), EventBus::noop())
            .get(&missing_task.task_id)
            .await
            .expect("reload missing correlated task after outcome")
            .is_none(),
        "missing task outcome must retain the expected task absence"
    );
    assert_eq!(
        format!("{:#?}", f.actor.active_refinements[&f.run_id]),
        format!("{projection_before:#?}"),
        "missing task outcome must preserve exact active projection"
    );
    assert_eq!(
        format!("{:#?}", f.actor.refinement_sessions[&f.run_id]),
        format!("{session_before:#?}"),
        "missing task outcome must preserve exact refinement session"
    );
    assert_rejected_outcome_skipped_reads(&f);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successor_persistence_failure_retains_exact_source_and_projection() {
    let mut f = durable_outcome_fixture().await;
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), f.projection.clone());
    f.actor
        .refinement_sessions
        .insert(f.run_id.clone(), f.session.clone());
    djinn_db::test_support::reject_refinement_successor_for_test(&f.db).await;

    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &f.session)
            .await,
        RefinementOutcomeApplication::Retryable
    );
    let projection = &f.actor.active_refinements[&f.run_id];
    assert_eq!(projection.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(projection.current_round, f.projection.current_round);
    assert_eq!(f.actor.refinement_sessions[&f.run_id].task_id, f.task_id);
    assert!(
        TaskRepository::new(f.db.clone(), EventBus::noop())
            .get(&f.task_id)
            .await
            .expect("reload exact source task")
            .is_some()
    );
    let durable = snapshot(&f).await;
    assert_eq!(durable.snapshot.run.state, RefinementRunState::Active);
    assert_eq!(
        durable.snapshot.intents.len(),
        1,
        "no successor was committed"
    );
    assert_eq!(
        durable.snapshot.intents[0].state,
        RefinementIntentState::Materialized,
        "source completion rolled back with successor insertion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_replay_of_non_current_source_intent_preserves_exact_state() {
    let mut f = durable_outcome_fixture().await;
    let source_projection = f.projection.clone();
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), source_projection.clone());
    f.actor
        .refinement_sessions
        .insert(f.run_id.clone(), f.session.clone());

    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &f.session)
            .await,
        RefinementOutcomeApplication::Committed
    );
    let committed = snapshot(&f).await;
    assert_eq!(
        committed.snapshot.intents.len(),
        2,
        "commit creates one successor"
    );
    assert_eq!(
        committed.snapshot.intents[0].state,
        RefinementIntentState::Completed
    );
    assert_eq!(
        f.actor.active_refinements[&f.run_id].phase,
        RefinementPhase::JudgeAdjudication,
        "the projection is published only after the source completion commits"
    );

    // A stale in-memory projection cannot turn the same completed source into a
    // second successor: the durable source-intent fence runs before outcome reads.
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), source_projection);
    let durable_before_replay = snapshot(&f).await;
    let task_before_replay = TaskRepository::new(f.db.clone(), EventBus::noop())
        .get(&f.task_id)
        .await
        .expect("load exact task before completed replay");
    let projection_before_replay = f.actor.active_refinements[&f.run_id].clone();
    let session_before_replay = f.actor.refinement_sessions[&f.run_id].clone();
    reset_rejected_outcome_counters(&f, &f.session.task_id);
    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &f.session)
            .await,
        RefinementOutcomeApplication::Ignored
    );
    let replayed = snapshot(&f).await;
    assert_eq!(
        replayed.snapshot.intents.len(),
        2,
        "replay creates no successor"
    );
    assert_exact_durable_run_and_intents(
        &replayed,
        &committed,
        "replay leaves committed durable state exact",
    );
    assert_exact_durable_run_and_intents(
        &replayed,
        &durable_before_replay,
        "completed replay preserves complete durable run and intent state",
    );
    assert_eq!(
        format!(
            "{:#?}",
            TaskRepository::new(f.db.clone(), EventBus::noop())
                .get(&f.task_id)
                .await
                .expect("reload exact task after completed replay")
        ),
        format!("{task_before_replay:#?}"),
        "completed replay preserves the exact durable task row"
    );
    assert_eq!(
        format!("{:#?}", f.actor.active_refinements[&f.run_id]),
        format!("{projection_before_replay:#?}"),
        "completed replay preserves the exact active projection"
    );
    assert_eq!(
        format!("{:#?}", f.actor.refinement_sessions[&f.run_id]),
        format!("{session_before_replay:#?}"),
        "completed replay preserves the exact refinement session"
    );
    assert_rejected_outcome_skipped_reads(&f);
}

async fn advance_fixture_to_judge(f: &mut DurableOutcomeFixture) {
    let repo = ProposalRepository::new(f.db.clone(), EventBus::noop());
    let next = repo
        .complete_refinement_intent(CompleteRefinementIntentRequest {
            run_id: f.run_id.clone(),
            intent_id: f.intent_id.clone(),
            generation: f.generation,
            owner: format!("coordinator:{}", f.actor.coordinator_incarnation_id),
            next_round: 1,
            next_phase: DurablePhase::JudgeAdjudication,
            next_role: RefinementRole::Judge,
            next_idempotency_key: format!("{}/1/judge_adjudication", f.run_id),
        })
        .await
        .expect("commit judge successor");
    let (task_id, session) = materialize_outcome_intent(
        &f.actor,
        &f.fixture,
        &f.run_id,
        f.generation,
        &next.intent_id,
        1,
        DurablePhase::JudgeAdjudication,
        RefinementRole::Judge,
    )
    .await;
    f.intent_id = next.intent_id;
    f.task_id = task_id;
    f.session = session;
    f.projection.phase = RefinementPhase::JudgeAdjudication;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_park_replay_is_fenced_before_a_second_transition() {
    let mut f = durable_outcome_fixture().await;
    advance_fixture_to_judge(&mut f).await;
    let metadata = serde_json::json!({
        "kind": "needs_evidence_link_v1",
        "proposal_id": f.fixture.proposal_id,
        "judge_task_id": f.task_id,
        "spike_task_id": uuid::Uuid::now_v7().to_string(),
        "round": 1,
        "against_revision_seq": f.projection.current_revision_seq,
    });
    ProposalRepository::new(f.db.clone(), EventBus::noop())
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &f.fixture.proposal_id,
            kind: "needs_evidence",
            body: "evidence required",
            blocking: true,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: Some(&f.task_id),
            against_revision_seq: f.projection.current_revision_seq,
            round: 1,
            body_metadata: Some(&metadata),
        })
        .await
        .expect("append needs-evidence decision");
    let source_projection = f.projection.clone();
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), source_projection.clone());

    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &f.session)
            .await,
        RefinementOutcomeApplication::Committed
    );
    assert_eq!(
        f.actor.active_refinements[&f.run_id].phase,
        RefinementPhase::AwaitingEvidence
    );
    let committed = snapshot(&f).await;
    assert_eq!(committed.snapshot.run.state, RefinementRunState::Parked);
    assert_eq!(
        committed
            .snapshot
            .intents
            .last()
            .expect("judge intent")
            .state,
        RefinementIntentState::Completed
    );

    // Restore the disposable projection: the durable run/intent must fence replay.
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), source_projection.clone());
    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &f.session)
            .await,
        RefinementOutcomeApplication::Ignored
    );
    assert_eq!(
        f.actor.active_refinements[&f.run_id].phase,
        source_projection.phase
    );
    let replayed = snapshot(&f).await;
    assert_eq!(replayed.snapshot.run.state, RefinementRunState::Parked);
    assert_eq!(
        replayed.snapshot.intents.len(),
        committed.snapshot.intents.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_terminal_replay_is_fenced_before_a_second_transition() {
    let mut f = durable_outcome_fixture().await;
    let source = SourceIntentTransitionRequest {
        run_id: f.run_id.clone(),
        intent_id: f.intent_id.clone(),
        generation: f.generation,
        expected_round: 1,
        expected_phase: DurablePhase::AdversaryAttack,
        expected_role: RefinementRole::Adversary,
    };
    let mut terminal_candidate = f.projection.clone();
    terminal_candidate.terminate(StopReason::HumanAccepted);
    assert!(
        f.actor
            .commit_refinement_candidate(&source, &terminal_candidate)
            .await,
        "terminal decision consumes its exact materialized source"
    );
    let committed = snapshot(&f).await;
    assert_eq!(committed.snapshot.run.state, RefinementRunState::Terminal);
    assert_eq!(committed.snapshot.intents.len(), 1);
    assert_eq!(
        committed.snapshot.intents[0].state,
        RefinementIntentState::Completed
    );

    f.actor
        .active_refinements
        .insert(f.run_id.clone(), f.projection.clone());
    assert_eq!(
        f.actor
            .process_refinement_outcome(&f.run_id, &f.session)
            .await,
        RefinementOutcomeApplication::Ignored
    );
    assert_eq!(
        f.actor.active_refinements[&f.run_id].phase,
        RefinementPhase::AdversaryAttack
    );
    let replayed = snapshot(&f).await;
    assert_eq!(replayed.snapshot.run.state, RefinementRunState::Terminal);
    assert_eq!(replayed.snapshot.intents.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advocate_proposal_and_progress_failures_leave_durable_source_retryable() {
    for point in [
        OutcomeTestSeamPoint::ProposalRead,
        OutcomeTestSeamPoint::DurableProgress,
    ] {
        let mut f = durable_outcome_fixture().await;
        f.actor
            .active_refinements
            .insert(f.run_id.clone(), f.projection.clone());
        f.actor
            .refinement_sessions
            .insert(f.run_id.clone(), f.session.clone());
        let mut candidate_source = f.projection.clone();
        // The fixture head is newer than this deliberately stale progress marker,
        // forcing the durable-progress boundary after the proposal reload.
        candidate_source.current_revision_seq = 0;
        let before = snapshot(&f).await;
        let lifecycle_before = ProposalRepository::new(f.db.clone(), EventBus::noop())
            .revisions(&f.fixture.proposal_id)
            .await
            .expect("read lifecycle before request failure")
            .into_iter()
            .filter(|revision| revision.event_kind == "refinement_stop")
            .count();
        inject_outcome_test_failure(&f.fixture.proposal_id, point);
        assert!(
            f.actor
                .process_advocate_outcome(
                    &f.run_id,
                    &f.fixture.proposal_id,
                    &f.task_id,
                    &candidate_source,
                )
                .await
                .is_none(),
            "{point:?} is retryable before publishing a candidate"
        );
        let mut after = snapshot(&f).await;
        // `observed_at` stamps the wall clock of the read itself, not durable
        // state, so two reads of an unchanged run differ whenever they land in
        // different milliseconds. Adopt the earlier stamp so the comparison
        // below asserts the invariant this matrix owns — every durable field is
        // untouched — instead of failing on read timing.
        after.observed_at = before.observed_at;
        assert_eq!(
            after, before,
            "{point:?} must leave durable run state untouched"
        );
        assert_eq!(f.actor.refinement_sessions[&f.run_id].task_id, f.task_id);
        assert_eq!(
            ProposalRepository::new(f.db.clone(), EventBus::noop())
                .revisions(&f.fixture.proposal_id)
                .await
                .expect("read lifecycle after request failure")
                .into_iter()
                .filter(|revision| revision.event_kind == "refinement_stop")
                .count(),
            lifecycle_before,
            "{point:?} must not append a proposal-scoped stop"
        );
        reset_outcome_test_seam(&f.fixture.proposal_id);
    }
}

/// Every terminal outcome consumes its exact source intent and never emits a
/// proposal-scoped compatibility lifecycle stop row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_typed_terminal_outcome_persists_once_on_its_exact_run() {
    let reasons = vec![
        StopReason::AdversaryDry,
        StopReason::RoundCap,
        StopReason::SpawnCap,
        StopReason::RepeatedObjection {
            signature: "same blocking objection".into(),
            occurrences: 2,
        },
        StopReason::AgentFailure {
            role: RefinementRole::Advocate,
            error_code: "advocate_failed".into(),
            message: "revision failed".into(),
        },
        StopReason::AgentFailure {
            role: RefinementRole::Adversary,
            error_code: "adversary_failed".into(),
            message: "attack failed".into(),
        },
        StopReason::AgentFailure {
            role: RefinementRole::Judge,
            error_code: "judge_failed".into(),
            message: "verdict failed".into(),
        },
        StopReason::HumanAccepted,
        StopReason::HumanRejected,
    ];

    for reason in reasons {
        let f = durable_outcome_fixture().await;
        let source = SourceIntentTransitionRequest {
            run_id: f.run_id.clone(),
            intent_id: f.intent_id.clone(),
            generation: f.generation,
            expected_round: 1,
            expected_phase: DurablePhase::AdversaryAttack,
            expected_role: RefinementRole::Adversary,
        };
        let mut candidate = f.projection.clone();
        candidate.terminate(reason.clone());
        assert!(
            f.actor
                .commit_refinement_candidate(&source, &candidate)
                .await
        );

        let committed = snapshot(&f).await;
        assert_eq!(committed.snapshot.run.state, RefinementRunState::Terminal);
        assert_eq!(committed.snapshot.run.terminal_reason, Some(reason.clone()));
        assert_eq!(
            committed.snapshot.intents[0].state,
            RefinementIntentState::Completed
        );
        assert!(
            ProposalRepository::new(f.db.clone(), EventBus::noop())
                .revisions(&f.fixture.proposal_id)
                .await
                .expect("read proposal revisions")
                .iter()
                .all(|revision| revision.event_kind != "refinement_stop"),
            "{reason:?} must not use proposal-scoped lifecycle persistence"
        );

        assert!(
            !f.actor
                .commit_refinement_candidate(&source, &candidate)
                .await
        );
        assert_eq!(
            snapshot(&f).await.snapshot.run.terminal_reason,
            Some(reason)
        );
    }
}

// ── Typed evidence role context (task tlyl) ──────────────────────────────

/// Raise a real typed demand on the fixture's proposal and return the finding.
///
/// The demand is written by the production writer
/// `set_structured_needs_evidence_spike`, so the block the coordinator renders
/// is a projection of authority the repository actually holds.
async fn demand_typed_evidence_for_outcome_fixture(f: &DurableOutcomeFixture) -> String {
    let project_id = ProposalRepository::new(f.db.clone(), EventBus::noop())
        .targets(&f.fixture.proposal_id)
        .await
        .expect("read outcome proposal targets")
        .first()
        .expect("outcome proposal has a target")
        .project_id
        .clone();
    let spike_task_id = djinn_db::test_support::seed_task_row(
        &f.db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: &project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    let repo = ProposalRepository::new(f.db.clone(), EventBus::noop());
    repo.set_structured_needs_evidence_spike(
        &f.fixture.proposal_id,
        &spike_task_id,
        &djinn_core::models::NeedsEvidenceClaim {
            question: "Can the launcher share a cgroup across pods?".into(),
            target_subsystem: "launcher".into(),
            spec_unknown_anchor: "cgroup delegation".into(),
            insufficient_in_session_research: "needs a live kernel probe".into(),
            expected_findings: "a delegated cgroup or a kernel refusal".into(),
            created_by_task_id: spike_task_id.clone(),
            round: 1,
            against_revision_seq: 1,
        },
    )
    .await
    .expect("raise a typed evidence demand");
    djinn_db::TypedEvidenceRepository::new(f.db.clone())
        .unresolved_projection(&f.fixture.proposal_id)
        .await
        .expect("project the demand")
        .expect("the demand is unresolved")
        .finding_id
}

/// Create one refinement task for `role` and return its persisted description.
async fn refinement_task_description(f: &DurableOutcomeFixture, role: &str) -> String {
    let repo = ProposalRepository::new(f.db.clone(), EventBus::noop());
    let head = repo
        .get(&f.fixture.proposal_id)
        .await
        .expect("load proposal")
        .expect("proposal exists")
        .latest_revision_seq;
    let task_id = f
        .actor
        .create_refinement_task_with_context_and_correlation(
            &f.fixture.proposal_id,
            role,
            1,
            head,
            "typed evidence role context",
            None,
            Some(&f.fixture.user_id),
            None,
        )
        .await
        .expect("create uncorrelated refinement task");
    TaskRepository::new(f.db.clone(), EventBus::noop())
        .get(&task_id)
        .await
        .expect("load created task")
        .expect("created task exists")
        .description
}

/// AC4 — Adversary and Judge descriptions carry the rendered typed block for a
/// proposal with an unresolved finding, and carry none when there is none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_role_context_reaches_every_tribunal_role() {
    let f = durable_outcome_fixture().await;

    // No finding yet: no block, for any role. This is the control — without it
    // the assertions below could pass on a block that is always injected.
    for role in ["adversary", "advocate", "judge"] {
        let description = refinement_task_description(&f, role).await;
        assert!(
            !description.contains("# Typed evidence"),
            "{role} must carry no typed block with no finding: {description}"
        );
    }

    let finding_id = demand_typed_evidence_for_outcome_fixture(&f).await;

    for role in ["adversary", "advocate", "judge"] {
        let description = refinement_task_description(&f, role).await;
        assert!(
            description.contains("# Typed evidence"),
            "{role} must carry the typed block: {description}"
        );
        assert!(
            description.contains(&finding_id),
            "{role} block must name the finding: {description}"
        );
        assert!(
            description.contains("Can the launcher share a cgroup across pods?"),
            "{role} block must carry the claim as one question: {description}"
        );
        assert!(
            description.contains("demanded against revision 1"),
            "{role} block must carry the originating revision: {description}"
        );
    }

    // Role scoping survives the coordinator, not just the renderer: the demand
    // surface reaches the Adversary and the Judge, never the Advocate.
    let advocate = refinement_task_description(&f, "advocate").await;
    let adversary = refinement_task_description(&f, "adversary").await;
    let judge = refinement_task_description(&f, "judge").await;
    assert!(
        !advocate.contains(djinn_roles::typed_evidence_context::DISPOSITION_SECTION),
        "the Advocate must not receive the disposition surface: {advocate}"
    );
    assert!(
        !adversary.contains(djinn_roles::typed_evidence_context::RETRY_SECTION),
        "the Adversary must not receive the retry surface: {adversary}"
    );
    assert_ne!(
        advocate, judge,
        "role scoping must produce different descriptions"
    );

    // A non-tribunal role gets no block at all.
    let worker = refinement_task_description(&f, "worker").await;
    assert!(
        !worker.contains("# Typed evidence"),
        "a non-tribunal role must carry no typed block: {worker}"
    );
}

/// AC5 — the legacy raw-`body_metadata` dump is gone. An `evidence_findings`
/// debate row no longer reaches any prompt; only the typed projection does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_evidence_findings_debate_row_no_longer_reaches_a_prompt() {
    let f = durable_outcome_fixture().await;
    let repo = ProposalRepository::new(f.db.clone(), EventBus::noop());
    let metadata = serde_json::json!({
        "answer": "legacy blob that must not be dumped into a prompt",
        "evidence": ["legacy blob that must not be dumped into a prompt"],
        "code_paths_inspected": [],
        "confidence": 0.0,
        "residual_risks": ["Structured V1 projection is authoritative."],
        "recommendation_for_advocate": "legacy blob that must not be dumped into a prompt",
    });
    repo.add_debate_trail_entry(djinn_db::ProposalDebateTrailCreateInput {
        proposal_id: &f.fixture.proposal_id,
        kind: "evidence_findings",
        body: "legacy findings body",
        blocking: false,
        agent_role: "spike",
        author_kind: "agent",
        author_model: None,
        source_task_id: None,
        against_revision_seq: 1,
        round: 1,
        body_metadata: Some(&metadata),
    })
    .await
    .expect("write a legacy evidence_findings row");

    let advocate = refinement_task_description(&f, "advocate").await;
    assert!(
        !advocate.contains("legacy blob that must not be dumped into a prompt"),
        "the raw body_metadata dump must be gone: {advocate}"
    );
    assert!(
        !advocate.contains("Structured findings metadata:"),
        "the legacy metadata section must be gone: {advocate}"
    );
    assert!(
        !advocate.contains("# Typed evidence"),
        "a legacy debate row must not fabricate a typed block: {advocate}"
    );
}
