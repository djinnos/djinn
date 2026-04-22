//! Property-based tests for the task state machine invariants.
//!
//! Uses `proptest` to explore the full status × action space, verifying
//! structural invariants that must hold for every well-formed input.
//!
//! All DB-backed cases use in-memory SQLite (no I/O) and run ≤6 transitions
//! per case; the full 256 × 5 suite completes well under 5 seconds.

use djinn_core::events::EventBus;
use djinn_core::models::{TaskStatus, TransitionAction, compute_transition};
use djinn_db::{Database, EpicRepository, Error, TaskRepository};
use proptest::prelude::*;

// ── Local test fixtures (pure djinn-db / djinn-core) ─────────────────────────

fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

fn test_events() -> EventBus {
    EventBus::noop()
}

// ── Runtime helper ────────────────────────────────────────────────────────────

fn rt() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    })
}

/// Stand up a fresh in-memory DB, create an epic, and return an `open` task.
async fn make_open_task() -> (TaskRepository, String) {
    let db = create_test_db();
    let epic = EpicRepository::new(db.clone(), test_events())
        .create("T", "", "", "", "", None)
        .await
        .unwrap();
    let repo = TaskRepository::new(db, test_events());
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
        Just(TaskStatus::Approved),
        Just(TaskStatus::PrDraft),
        Just(TaskStatus::PrReview),
        Just(TaskStatus::NeedsLeadIntervention),
        Just(TaskStatus::InLeadIntervention),
        Just(TaskStatus::Closed),
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
        Just(TransitionAction::LeadInterventionStart),
        Just(TransitionAction::LeadInterventionRelease),
        Just(TransitionAction::LeadInterventionComplete),
        Just(TransitionAction::LeadApprove),
        Just(TransitionAction::LeadApproveConflict),
        Just(TransitionAction::PrCreated),
        Just(TransitionAction::PrUndraft),
        Just(TransitionAction::PrCiFailed),
        Just(TransitionAction::PrConflict),
        Just(TransitionAction::PrMerge),
        Just(TransitionAction::PrChangesRequested),
    ]
}

// ── DB-backed exhaustive tests (plain #[test], not proptest) ─────────────────
// With only 8 non-closed statuses there is no sampling benefit — just iterate.

const NON_CLOSED_STATUSES: &[TaskStatus] = &[
    TaskStatus::Open,
    TaskStatus::InProgress,
    TaskStatus::Verifying,
    TaskStatus::NeedsTaskReview,
    TaskStatus::InTaskReview,
    TaskStatus::Approved,
    TaskStatus::PrDraft,
    TaskStatus::PrReview,
    TaskStatus::NeedsLeadIntervention,
    TaskStatus::InLeadIntervention,
];

#[test]
fn closed_at_set_on_close_cleared_on_reopen() {
    rt().block_on(async {
        for from in NON_CLOSED_STATUSES {
            let status_str = from.as_str();
            let (repo, id) = make_open_task().await;
            repo.set_status(&id, status_str).await.unwrap();
            let after_close = repo
                .transition(&id, TransitionAction::Close, "", "system", None, None)
                .await
                .unwrap_or_else(|_| panic!("Close failed from {status_str}"));
            assert!(
                after_close.closed_at.is_some(),
                "closed_at must be set after Close from {status_str}"
            );
            let after_reopen = repo
                .transition(
                    &id,
                    TransitionAction::Reopen,
                    "",
                    "system",
                    Some("test"),
                    None,
                )
                .await
                .unwrap_or_else(|_| panic!("Reopen failed from closed (was {status_str})"));
            assert!(
                after_reopen.closed_at.is_none(),
                "closed_at must be None after Reopen (was {status_str})"
            );
        }
    });
}

#[test]
fn reopen_count_monotonically_increases() {
    rt().block_on(async {
        let (repo, id) = make_open_task().await;
        let mut prev = 0i64;
        for cycle in 0..6 {
            repo.transition(&id, TransitionAction::Close, "", "system", None, None)
                .await
                .unwrap_or_else(|_| panic!("Close failed on cycle {cycle}"));
            let t = repo
                .transition(
                    &id,
                    TransitionAction::Reopen,
                    "",
                    "system",
                    Some("cycle"),
                    None,
                )
                .await
                .unwrap_or_else(|_| panic!("Reopen failed on cycle {cycle}"));
            assert!(
                t.reopen_count > prev,
                "reopen_count must increase: {prev} -> {}",
                t.reopen_count
            );
            prev = t.reopen_count;
        }
    });
}

#[test]
fn double_close_returns_error() {
    rt().block_on(async {
        for from in NON_CLOSED_STATUSES {
            let status_str = from.as_str();
            let (repo, id) = make_open_task().await;
            repo.set_status(&id, status_str).await.unwrap();
            repo.transition(&id, TransitionAction::Close, "", "system", None, None)
                .await
                .unwrap_or_else(|_| panic!("First Close failed from {status_str}"));
            let err = repo
                .transition(&id, TransitionAction::Close, "", "system", None, None)
                .await
                .expect_err("second Close must return an error");
            assert!(
                matches!(err, Error::InvalidTransition(_)),
                "expected InvalidTransition, got {err:?} (from {status_str})"
            );
        }
    });
}

// ── Property tests (pure logic, no DB — 256 random samples worthwhile) ───────

proptest! {
    // 3. UserOverride accepts every (from, to) status pair — no Err path.
    #[test]
    fn user_override_accepts_any_status_pair(from in arb_status(), to in arb_status()) {
        let result = compute_transition(&TransitionAction::UserOverride, &from, Some(&to));
        prop_assert!(result.is_ok(), "user_override must succeed for any (from, to) pair");
        let apply = result.unwrap();
        prop_assert_eq!(apply.to_status, Some(to));
    }

    // 4. Every successful compute_transition produces a to_status whose as_str()
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
