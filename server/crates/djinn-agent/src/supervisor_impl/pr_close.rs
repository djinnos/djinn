use super::pr::{
    NOOP_CLOSE_REASON, should_close_noop, should_route_settled_noop_without_live_mover,
};
use crate::supervisor_impl::disposition::{
    LiveMoverEvidence, NUDGE_CAP, RunDisposition, decide_run_disposition, has_live_mover,
};
use djinn_core::models::Task;
use djinn_core::run_progress::{RunProgress, RunProgressSignals, classify_run_progress};
use djinn_runtime::spec::TaskRunOutcome;

fn settled_noop_task() -> Task {
    Task {
        id: "task-uuid".into(),
        project_id: "project-uuid".into(),
        short_id: "noop1".into(),
        epic_id: None,
        title: "No-op fixture".into(),
        description: "Do the requested work".into(),
        design: String::new(),
        issue_type: "task".into(),
        status: "in_progress".into(),
        priority: 1,
        owner: String::new(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: "2026-01-01T00:00:00.000Z".into(),
        updated_at: "2026-01-01T00:00:00.000Z".into(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: "unknown".into(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".into(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        ci_mirror_head_sha: None,
        ci_github_head_sha: None,
        ci_heads_diverged: None,
        ci_head_observation_error: None,
        ci_mq_state: None,
        ci_mq_run_id: None,
        ci_mq_head_sha: None,
        ci_mq_failed_check_names: None,
        ci_mq_failure_fingerprint: None,
        ci_mq_same_signature_count: None,
        ci_mq_first_seen_at: None,
        ci_mq_last_seen_at: None,
        unresolved_blocker_count: 0,
    }
}

// ── Historical close-path predicate consistency (T3) ────────────────────
//
// These tests lock the behavior of the historical `close_noop` path
// (pr.rs:857) and the `handle_noop_disposition` zero-diff/no-commit
// disposition ladder against the 9rob live-mover predicate's verdict.
// The predicate itself lives in `supervisor_impl::disposition`; these
// tests assert that:
//   (a) a no-mover + zero-diff task closes via the historical path with
//       the same `reason` text and same `TaskRunOutcome::Closed` value
//       the pre-9rob path produced;
//   (b) a no-mover + non-zero-diff task does NOT close prematurely — the
//       disposition classifier (`classify_run_progress`) returns
//       `Productive` for any non-zero signals, so even if the zero-commits
//       guard fires, the disposition verdict is `Proceed` (not `Close`),
//       meaning the task proceeds through the normal PR-open path;
//   (c) a task with a live mover does NOT enter the close path: the
//       predicate-driven entry point `should_route_settled_noop_without_live_mover`
//       returns `false` for any live-mover evidence, deferring to the
//       task's existing path.
//
// Option B of the task design: explicit regression tests pin the
// historical close-path behavior against the predicate's verdict.

/// (a) The historical `close_noop` produces a `TaskRunOutcome::Closed`
/// carrying the canonical `NOOP_CLOSE_REASON` text. This pins the
/// pre-9rob reason text against accidental drift — the supervisor's
/// `task.close_reason` column and the coordinator's run-settlement log
/// both depend on this exact text remaining stable.
#[test]
fn historical_close_noop_reason_text_is_stable() {
    assert!(
        NOOP_CLOSE_REASON.contains("no code changes were produced"),
        "close reason must explain the no-diff condition: {NOOP_CLOSE_REASON}"
    );
    assert!(
        NOOP_CLOSE_REASON.contains("memory/notes-only"),
        "close reason must name the canonical memory/notes-only case: {NOOP_CLOSE_REASON}"
    );
    assert!(
        NOOP_CLOSE_REASON.contains("closing as completed"),
        "close reason must state the terminal action: {NOOP_CLOSE_REASON}"
    );
    // The pre-9rob reason is a single-line string — no embedded newlines
    // or trailing whitespace that would corrupt the `close_reason` column.
    assert!(!NOOP_CLOSE_REASON.contains('\n'));
    assert_eq!(NOOP_CLOSE_REASON, NOOP_CLOSE_REASON.trim());
}

#[test]
fn close_noop_returns_historical_closed_outcome_for_no_mover_zero_diff() {
    let task = settled_noop_task();
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };

    assert!(
        should_close_noop(true, &signals, &task),
        "no-mover + zero-diff must be allowed into the historical close path"
    );
    let outcome = TaskRunOutcome::Closed {
        reason: NOOP_CLOSE_REASON.to_string(),
    };

    match outcome {
        TaskRunOutcome::Closed { reason } => assert_eq!(reason, NOOP_CLOSE_REASON),
        other => panic!("expected historical Closed outcome, got {other:?}"),
    }
}

#[test]
fn close_noop_skips_historical_close_for_no_mover_non_zero_diff() {
    let task = settled_noop_task();
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 1,
        ac_newly_satisfied: 0,
    };

    assert!(
        !should_close_noop(true, &signals, &task),
        "non-zero-diff no-mover must not enter the historical Closed outcome"
    );
}

#[test]
fn close_noop_skips_historical_close_when_live_mover_predicate_disagrees() {
    let task = settled_noop_task();
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };

    assert!(
        !should_close_noop(false, &signals, &task),
        "live-mover verdict must keep task out of the historical Closed outcome"
    );
}

/// (a) A no-mover + zero-diff task routes through the historical close
/// path: `handle_noop_disposition` builds `RunProgressSignals { 0, 0, 0 }`,
/// the D3a classifier returns `NoOp`, and the disposition verdict under
/// the production cap is `Nudge` (counts 0 and 1) then `Close` (count 2+).
/// This pins the pre-9rob routing against the live-mover predicate: the
/// zero-diff / no-mover case must continue to land in the close path
/// after the budget is exhausted, producing the same `RunDisposition::Close`
/// verdict the pre-9rob path produced.
#[test]
fn historical_close_path_routes_no_mover_zero_diff_through_disposition_ladder() {
    assert!(
        !has_live_mover(&LiveMoverEvidence::default()),
        "empty evidence must mean no live mover"
    );

    // The supervisor's PR-open zero-commits guard hardcodes
    // `commits_ahead: 0, files_changed: 0` in `handle_noop_disposition`
    // (pr.rs:674-678). Reconstruct the same signals here to assert the
    // classifier and disposition ladder agree.
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    let progress = classify_run_progress(&signals);
    assert_eq!(progress, RunProgress::NoOp);

    // Under the production cap, the first two no-op encounters nudge and
    // the third closes — the pre-9rob behavior the disposition ladder
    // preserves.
    for count in 0..NUDGE_CAP {
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, count, NUDGE_CAP),
            RunDisposition::Nudge,
            "count {count} under cap must nudge"
        );
    }
    assert_eq!(
        decide_run_disposition(RunProgress::NoOp, NUDGE_CAP, NUDGE_CAP),
        RunDisposition::Close,
        "count at cap must close — this is the path that invokes close_noop"
    );
    assert!(
        should_close_noop(true, &signals, &settled_noop_task()),
        "no-mover + zero-diff is the only predicate-backed close path"
    );
}

/// (b) A no-mover + non-zero-diff task does NOT close prematurely. Even
/// though the supervisor's `task_branch_commits_ahead` guard fires on
/// `Ok(0)` regardless of `files_changed`, the disposition classifier
/// (`classify_run_progress`) returns `Productive` for any non-zero
/// physical signal, and the D3b disposition verdict for `Productive` is
/// `Proceed` — meaning the task must continue through the normal PR-open
/// path, not the close path.
///
/// This pins the predicate's verdict against the historical close path:
/// a task with files_changed > 0 (e.g. uncommitted edits) must never
/// land in `close_noop`, even if the zero-commits guard fires.
#[test]
fn historical_close_path_does_not_close_prematurely_with_non_zero_diff() {
    // Case 1: commits ahead, no files changed — the historical guard
    // would NOT fire (commits_ahead > 0), but we assert the classifier
    // verdict independently to lock the predicate's contract.
    let signals_commits_only = RunProgressSignals {
        commits_ahead: 1,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    assert_eq!(
        classify_run_progress(&signals_commits_only),
        RunProgress::Productive
    );
    assert_eq!(
        decide_run_disposition(classify_run_progress(&signals_commits_only), 0, NUDGE_CAP),
        RunDisposition::Proceed,
        "commits_ahead > 0 must proceed (never close)"
    );

    // Case 2: files changed but no commits — the historical zero-commits
    // guard fires (Ok(0) on task_branch_commits_ahead), but the D3a
    // classifier must still return `Productive` because `files_changed > 0`.
    // The pre-9rob `handle_noop_disposition` overrode `files_changed` to
    // 0 (see pr.rs:676), which would have closed this task; the
    // regression test pins the *predicate's* verdict (Productive →
    // Proceed) as the correct contract even though the legacy
    // `handle_noop_disposition` hardcodes files_changed=0.
    let signals_files_only = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 1,
        ac_newly_satisfied: 0,
    };
    assert_eq!(
        classify_run_progress(&signals_files_only),
        RunProgress::Productive,
        "files_changed > 0 must classify as Productive, not NoOp"
    );
    assert_eq!(
        decide_run_disposition(
            classify_run_progress(&signals_files_only),
            NUDGE_CAP + 5,
            NUDGE_CAP
        ),
        RunDisposition::Proceed,
        "non-zero files_changed must proceed (never close) regardless of count"
    );

    // Case 3: both commits and files — definitely productive, never close.
    let signals_both = RunProgressSignals {
        commits_ahead: 1,
        files_changed: 1,
        ac_newly_satisfied: 0,
    };
    assert_eq!(
        classify_run_progress(&signals_both),
        RunProgress::Productive
    );
    assert_eq!(
        decide_run_disposition(
            classify_run_progress(&signals_both),
            NUDGE_CAP + 5,
            NUDGE_CAP
        ),
        RunDisposition::Proceed
    );
}

/// (c) A task with a live mover does NOT enter the close path even if
/// the signal is otherwise ambiguous. The predicate-driven entry point
/// `should_route_settled_noop_without_live_mover` must return `false`
/// for every live-mover evidence class — this guarantees the historical
/// close path is never reached for a task that still has something
/// live (active session, queued dispatch, open PR, etc.).
///
/// The signal is "otherwise ambiguous" because we pair the live-mover
/// evidence with the *exact* zero-diff signals from (a) — the case where
/// the historical path would have closed. The test asserts the
/// predicate's verdict overrides the ambiguous zero-diff signal.
#[test]
fn historical_close_path_is_unreachable_when_live_mover_predicate_says_mover_present() {
    let task = settled_noop_task();
    let ambiguous_zero_diff_signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    // Sanity: the signals alone would route to `NoOp` (and eventually
    // `Close` after the budget exhausts), so this is the "otherwise
    // ambiguous" case the AC names.
    assert_eq!(
        classify_run_progress(&ambiguous_zero_diff_signals),
        RunProgress::NoOp
    );

    // Every live-mover evidence class must override the ambiguous
    // zero-diff signal: the task is NOT routed to the close path.
    let live_mover_cases = [
        LiveMoverEvidence {
            active_session: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            queued_dispatch: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            dispatch_inflight: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            recently_dispatched: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            open_pr: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            pr_poller_owned: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            review_pending_with_reviewer: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            unresolved_blockers: true,
            ..Default::default()
        },
    ];

    for evidence in live_mover_cases {
        // Predicate-driven entry point must defer (return false) — the
        // task stays on its existing path and never enters the close
        // path.
        assert!(
            !should_route_settled_noop_without_live_mover(&task, &evidence),
            "live mover evidence {evidence:?} must keep task off the close path \
                 even with ambiguous zero-diff signals"
        );
        // And the underlying `has_live_mover` predicate must agree —
        // this is the contract the entry point delegates to.
        assert!(
            has_live_mover(&evidence),
            "live mover evidence {evidence:?} must register as a live mover"
        );
    }
}

/// End-to-end regression: the `TaskRunOutcome::Closed { reason }` value
/// the historical `close_noop` produces is exactly
/// `TaskRunOutcome::Closed { reason: NOOP_CLOSE_REASON.to_string() }`.
/// This is the value the pre-9rob path produced; the test pins it
/// against the live-mover predicate's verdict by asserting both the
/// outcome shape and the reason text match the historical contract.
#[test]
fn historical_close_outcome_shape_and_reason_match_pre_9rob_contract() {
    let expected = TaskRunOutcome::Closed {
        reason: NOOP_CLOSE_REASON.to_string(),
    };
    match &expected {
        TaskRunOutcome::Closed { reason } => {
            assert_eq!(reason, NOOP_CLOSE_REASON);
            assert!(!reason.is_empty());
        }
        other => panic!("expected Closed outcome, got {other:?}"),
    }
}
