use super::*;

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
        Err(super::super::refinement::StopReason::AgentFailure { ref role, ref error })
            if role == "advocate" && error.contains("SPEC_LINT_REJECTED")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successor_persistence_failure_retains_exact_source_and_projection() {
    let mut f = durable_outcome_fixture().await;
    f.actor
        .active_refinements
        .insert(f.run_id.clone(), f.projection.clone());
    f.actor
        .refinement_sessions
        .insert(f.run_id.clone(), f.session.clone());
    sqlx::query("CREATE FUNCTION reject_refinement_successor_for_test() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected successor persistence failure'; END; $$ LANGUAGE plpgsql")
        .execute(f.db.pool()).await.expect("create successor failure function");
    sqlx::query("CREATE TRIGGER reject_refinement_successor_for_test BEFORE INSERT ON refinement_dispatch_intents FOR EACH ROW EXECUTE FUNCTION reject_refinement_successor_for_test()")
        .execute(f.db.pool()).await.expect("install successor failure trigger");

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
