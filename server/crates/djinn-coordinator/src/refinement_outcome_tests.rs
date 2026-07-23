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

#[test]
fn production_path_commits_successor_before_projection_publication() {
    let source = include_str!("refinement_outcome.rs");
    let commit = source
        .find("commit_refinement_candidate(&source, &candidate)")
        .expect("candidate must cross the durable commit boundary");
    let publish = source
        .find(".insert(run_id.to_owned(), candidate.clone())")
        .expect("committed candidate must be published");
    assert!(commit < publish, "durable commit must precede publication");
    assert!(source.contains("complete_refinement_intent(CompleteRefinementIntentRequest"));
}

#[test]
fn park_and_terminal_outcomes_consume_the_exact_source_intent() {
    let source = include_str!("refinement_outcome.rs");
    assert!(source.contains("park_refinement_run_from_intent"));
    assert!(source.contains("terminal_refinement_run_from_intent"));
    assert!(source.contains("source: source.clone()"));
}
