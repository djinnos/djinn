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
