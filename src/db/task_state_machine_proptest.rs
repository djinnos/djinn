//! Property-based tests for the task state machine invariants.
//!
//! Uses `proptest` to explore the full status × action space, verifying
//! structural invariants that must hold for every well-formed input.
//!
//! All DB-backed cases use in-memory SQLite (no I/O) and run ≤6 transitions
//! per case; the full 256 × 5 suite completes well under 5 seconds.

use djinn_core::models::{TaskStatus, TransitionAction, compute_transition};
use djinn_db::{EpicRepository, Error, TaskRepository};
use proptest::prelude::*;

use crate::test_helpers;

// ── Runtime helper ────────────────────────────────────────────────────────────

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Stand up a fresh in-memory DB, create an epic, and return an `open` task.
async fn make_open_task() -> (TaskRepository, String) {
    let db = test_helpers::create_test_db();
    let epic = EpicRepository::new(db.clone(), test_helpers::test_events())
        .create("T", "", "", "", "", None)
        .await
        .unwrap();
    let repo = TaskRepository::new(db, test_helpers::test_events());
    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    (repo, task.id)
}

// ── Strategies ────────────────────────────────────────────────────────────────

fn arb_status() -> impl Strategy<Value = TaskStatus> {
    prop_oneof![
        Just(TaskStatus::Open),
        Just(TaskStatus::InProgress),
        Just(TaskStatus::Verifying),
        Just(TaskStatus::NeedsTaskReview),
        Just(TaskStatus::InTaskReview),
        Just(TaskStatus::NeedsPmIntervention),
        Just(TaskStatus::InPmIntervention),
        Just(TaskStatus::Closed),
    ]
}

fn arb_non_closed_status() -> impl Strategy<Value = TaskStatus> {
    prop_oneof![
        Just(TaskStatus::Open),
        Just(TaskStatus::InProgress),
        Just(TaskStatus::Verifying),
        Just(TaskStatus::NeedsTaskReview),
        Just(TaskStatus::InTaskReview),
        Just(TaskStatus::NeedsPmIntervention),
        Just(TaskStatus::InPmIntervention),
    ]
}

fn arb_action() -> impl Strategy<Value = TransitionAction> {
    prop_oneof![
        Just(TransitionAction::Start),
        Just(TransitionAction::SubmitVerification),
        Just(TransitionAction::VerificationPass),
        Just(TransitionAction::VerificationFail),
        Just(TransitionAction::ReleaseVerification),
        Just(TransitionAction::SubmitTaskReview),
        Just(TransitionAction::TaskReviewStart),
        Just(TransitionAction::TaskReviewReject),
        Just(TransitionAction::TaskReviewRejectStale),
        Just(TransitionAction::TaskReviewRejectConflict),
        Just(TransitionAction::TaskReviewApprove),
        Just(TransitionAction::Close),
        Just(TransitionAction::Reopen),
        Just(TransitionAction::Release),
        Just(TransitionAction::ReleaseTaskReview),
        Just(TransitionAction::ForceClose),
        Just(TransitionAction::UserOverride),
        Just(TransitionAction::Escalate),
        Just(TransitionAction::PmInterventionStart),
        Just(TransitionAction::PmInterventionRelease),
        Just(TransitionAction::PmInterventionComplete),
        Just(TransitionAction::PmApprove),
        Just(TransitionAction::PmApproveConflict),
    ]
}

// ── Property tests ────────────────────────────────────────────────────────────

proptest! {
    // 1. closed_at is set after Close and cleared after Reopen, for any starting status.
    #[test]
    fn closed_at_set_on_close_cleared_on_reopen(from in arb_non_closed_status()) {
        let (has_closed_at, cleared_after_reopen) = rt().block_on(async move {
            let (repo, id) = make_open_task().await;
            repo.set_status(&id, from.as_str()).await.unwrap();
            let after_close = repo
                .transition(&id, TransitionAction::Close, "", "system", None, None)
                .await
                .unwrap();
            let after_reopen = repo
                .transition(&id, TransitionAction::Reopen, "", "system", Some("test"), None)
                .await
                .unwrap();
            (after_close.closed_at.is_some(), after_reopen.closed_at.is_none())
        });
        prop_assert!(has_closed_at, "closed_at must be set after Close");
        prop_assert!(cleared_after_reopen, "closed_at must be None after Reopen");
    }

    // 2. reopen_count strictly increases on every close/reopen cycle.
    #[test]
    fn reopen_count_monotonically_increases(n_cycles in 3usize..=6) {
        let counts: Vec<i64> = rt().block_on(async move {
            let (repo, id) = make_open_task().await;
            let mut counts = Vec::with_capacity(n_cycles);
            for _ in 0..n_cycles {
                repo.transition(&id, TransitionAction::Close, "", "system", None, None)
                    .await
                    .unwrap();
                let t = repo
                    .transition(&id, TransitionAction::Reopen, "", "system", Some("cycle"), None)
                    .await
                    .unwrap();
                counts.push(t.reopen_count);
            }
            counts
        });
        prop_assert_eq!(counts.len(), n_cycles);
        for window in counts.windows(2) {
            prop_assert!(
                window[1] > window[0],
                "reopen_count must increase: {} -> {}",
                window[0],
                window[1]
            );
        }
    }

    // 3. UserOverride accepts every (from, to) status pair — no Err path.
    #[test]
    fn user_override_accepts_any_status_pair(from in arb_status(), to in arb_status()) {
        let result = compute_transition(&TransitionAction::UserOverride, &from, Some(&to));
        prop_assert!(result.is_ok(), "user_override must succeed for any (from, to) pair");
        let apply = result.unwrap();
        prop_assert_eq!(apply.to_status, Some(to));
    }

    // 4. Attempting to close an already-closed task returns InvalidTransition, never panics.
    #[test]
    fn double_close_returns_error(from in arb_non_closed_status()) {
        let second_close = rt().block_on(async move {
            let (repo, id) = make_open_task().await;
            repo.set_status(&id, from.as_str()).await.unwrap();
            repo.transition(&id, TransitionAction::Close, "", "system", None, None)
                .await
                .unwrap();
            repo.transition(&id, TransitionAction::Close, "", "system", None, None)
                .await
        });
        prop_assert!(second_close.is_err(), "second Close must return an error");
        prop_assert!(
            matches!(second_close.unwrap_err(), Error::InvalidTransition(_)),
            "error must be InvalidTransition"
        );
    }

    // 5. Every successful compute_transition produces a to_status whose as_str()
    //    round-trips through TaskStatus::parse without error.
    #[test]
    fn successful_transition_gives_parseable_status(
        action in arb_action(),
        from in arb_status(),
        to in arb_status(),
    ) {
        let target = if action == TransitionAction::UserOverride {
            Some(to)
        } else {
            None
        };
        if let Ok(apply) = compute_transition(&action, &from, target.as_ref()) {
            let to_status = apply.to_status.expect("successful transition must set to_status");
            let s = to_status.as_str();
            let reparsed = TaskStatus::parse(s)
                .expect("to_status.as_str() must yield a parseable status string");
            prop_assert_eq!(reparsed, to_status);
        }
    }
}
