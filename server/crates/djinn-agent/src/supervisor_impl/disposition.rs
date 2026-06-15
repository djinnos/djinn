//! D3b/D3c: evidence-based disposition for a "succeeded but did nothing" run.
//!
//! When the supervisor reaches the PR-open guard with a task branch that is
//! **zero commits ahead** of the merge target, the run reported done but left
//! nothing to open a PR with. Historically that case force-closed the task as
//! completed (see the no-commits guard in [`super::pr`]). That is the right
//! call once we've actually given the run a fair chance — but blindly closing
//! the *first* time a worker exits empty throws away the task on a single
//! flubbed turn.
//!
//! This module factors that decision into a pure predicate so it's testable in
//! isolation and shares one definition of "did the run make progress?" with
//! [`djinn_core::run_progress`] (D3a):
//!
//! - A `Productive` run (any commit ahead / any file changed) never reaches
//!   here — the caller opens the PR. The predicate still models it for
//!   totality: it proceeds (no nudge, no close).
//! - A `NoOp` run (no physical change, no acceptance-criteria progress) gets a
//!   **bounded** corrective nudge: re-dispatch with a hint, up to
//!   [`NUDGE_CAP`] times, *then* close. The bound rides the task's existing
//!   `continuation_count` (the same counter the stale-rework loop uses), so the
//!   nudge is idempotent per dispatch — each nudge increments the counter and
//!   spawns a fresh run, so the same finished run can never be nudged twice.
//! - An `Inconclusive` run (acceptance criteria flipped to met but no physical
//!   change) is deliberately **not** auto-nudged: evidence and bookkeeping
//!   disagree, so we preserve the conservative status-quo close rather than
//!   spend more dispatches on an ambiguous signal.
//!
//! The predicate is intentionally side-effect free: it only reads the
//! classified [`RunProgress`] and the current `continuation_count`, and returns
//! what the caller should *do*. All DB writes (increment, transition, comment)
//! stay in the caller.

use djinn_core::run_progress::RunProgress;

/// Maximum number of corrective nudges a single task may receive before the
/// no-op disposition falls through to a terminal close. Two — generous enough
/// to recover a worker that flubbed once or twice, bounded so a structurally
/// stuck task can't loop forever.
pub(crate) const NUDGE_CAP: i64 = 2;

/// What the caller should do with a finished run at the no-op disposition fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunDisposition {
    /// The run made real progress — proceed with the normal terminal action
    /// (open the PR). The caller reaches this only when classification says
    /// `Productive`; modelled here for totality.
    Proceed,
    /// The run did nothing, but it is still under the nudge budget: give it a
    /// bounded corrective nudge (re-dispatch with a hint) instead of closing.
    Nudge,
    /// The run did nothing and the nudge budget is exhausted (or the signal is
    /// ambiguous): fall through to the existing terminal close.
    Close,
}

/// Explicit supervisor evidence that a task still has something live that can
/// move it forward.
///
/// This model is deliberately a bag of already-collected facts. It performs no
/// database, repository, GitHub, or LLM/text judgement itself; callers populate
/// the fields from their own hard evidence sources and then use
/// [`has_live_mover`] or [`live_mover_reasons`] as a pure predicate.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LiveMoverEvidence {
    /// A worker/reviewer session is currently active for the task.
    pub active_session: bool,
    /// The task is queued for dispatch but the dispatch has not started yet.
    pub queued_dispatch: bool,
    /// A dispatch for the task is currently being started or handed off.
    pub dispatch_inflight: bool,
    /// The coordinator remembers a recent dispatch marker for the task.
    pub recently_dispatched: bool,
    /// The task has an open PR capable of receiving review or CI movement.
    pub open_pr: bool,
    /// A PR poller owns/watches the task's PR lifecycle.
    pub pr_poller_owned: bool,
    /// The task is waiting on a human/system reviewer rather than abandoned.
    pub review_pending_with_reviewer: bool,
    /// The task is blocked by unresolved blocker edges that can still clear.
    pub unresolved_blockers: bool,
}

/// One explicit fact explaining why [`has_live_mover`] returned true.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveMoverReason {
    ActiveSession,
    QueuedDispatch,
    DispatchInflight,
    RecentlyDispatched,
    OpenPr,
    PrPollerOwned,
    ReviewPendingWithReviewer,
    UnresolvedBlockers,
}

/// Return every explicit evidence class that says this task still has a live
/// mover. The order is stable and follows the field order of
/// [`LiveMoverEvidence`] so callers can present deterministic explanations.
#[allow(dead_code)]
pub(crate) fn live_mover_reasons(evidence: &LiveMoverEvidence) -> Vec<LiveMoverReason> {
    let mut reasons = Vec::new();

    if evidence.active_session {
        reasons.push(LiveMoverReason::ActiveSession);
    }
    if evidence.queued_dispatch {
        reasons.push(LiveMoverReason::QueuedDispatch);
    }
    if evidence.dispatch_inflight {
        reasons.push(LiveMoverReason::DispatchInflight);
    }
    if evidence.recently_dispatched {
        reasons.push(LiveMoverReason::RecentlyDispatched);
    }
    if evidence.open_pr {
        reasons.push(LiveMoverReason::OpenPr);
    }
    if evidence.pr_poller_owned {
        reasons.push(LiveMoverReason::PrPollerOwned);
    }
    if evidence.review_pending_with_reviewer {
        reasons.push(LiveMoverReason::ReviewPendingWithReviewer);
    }
    if evidence.unresolved_blockers {
        reasons.push(LiveMoverReason::UnresolvedBlockers);
    }

    reasons
}

/// Pure predicate: true when any explicit live-mover evidence is present.
#[allow(dead_code)]
pub(crate) fn has_live_mover(evidence: &LiveMoverEvidence) -> bool {
    evidence.active_session
        || evidence.queued_dispatch
        || evidence.dispatch_inflight
        || evidence.recently_dispatched
        || evidence.open_pr
        || evidence.pr_poller_owned
        || evidence.review_pending_with_reviewer
        || evidence.unresolved_blockers
}

/// Decide the disposition of a finished run from its classified progress and
/// how many nudges it has already consumed.
///
/// Pure and deterministic — a function of its inputs alone.
///
/// Decision order:
/// 1. `Productive` → [`RunDisposition::Proceed`] (never close real work).
/// 2. `NoOp` with `continuation_count < nudge_cap` → [`RunDisposition::Nudge`].
/// 3. `NoOp` at/over the cap → [`RunDisposition::Close`].
/// 4. `Inconclusive` → [`RunDisposition::Close`] (don't auto-nudge an
///    ambiguous signal; preserve the conservative status-quo close).
pub(crate) fn decide_run_disposition(
    progress: RunProgress,
    continuation_count: i64,
    nudge_cap: i64,
) -> RunDisposition {
    match progress {
        RunProgress::Productive => RunDisposition::Proceed,
        RunProgress::NoOp => {
            if continuation_count < nudge_cap {
                RunDisposition::Nudge
            } else {
                RunDisposition::Close
            }
        }
        RunProgress::Inconclusive => RunDisposition::Close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_single_live_mover_reason(
        evidence: LiveMoverEvidence,
        expected_reason: LiveMoverReason,
    ) {
        assert!(has_live_mover(&evidence));
        assert_eq!(live_mover_reasons(&evidence), vec![expected_reason]);
    }

    // ── Live mover evidence: pure hard-fact predicate ───────────────────────

    #[test]
    fn active_session_is_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                active_session: true,
                ..Default::default()
            },
            LiveMoverReason::ActiveSession,
        );
    }

    #[test]
    fn queued_dispatch_is_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                queued_dispatch: true,
                ..Default::default()
            },
            LiveMoverReason::QueuedDispatch,
        );
    }

    #[test]
    fn dispatch_inflight_is_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                dispatch_inflight: true,
                ..Default::default()
            },
            LiveMoverReason::DispatchInflight,
        );
    }

    #[test]
    fn remembered_recent_dispatch_is_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                recently_dispatched: true,
                ..Default::default()
            },
            LiveMoverReason::RecentlyDispatched,
        );
    }

    #[test]
    fn open_pr_is_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                open_pr: true,
                ..Default::default()
            },
            LiveMoverReason::OpenPr,
        );
    }

    #[test]
    fn pr_poller_ownership_is_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                pr_poller_owned: true,
                ..Default::default()
            },
            LiveMoverReason::PrPollerOwned,
        );
    }

    #[test]
    fn review_pending_with_reviewer_is_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                review_pending_with_reviewer: true,
                ..Default::default()
            },
            LiveMoverReason::ReviewPendingWithReviewer,
        );
    }

    #[test]
    fn unresolved_blocker_edges_are_live_mover() {
        assert_single_live_mover_reason(
            LiveMoverEvidence {
                unresolved_blockers: true,
                ..Default::default()
            },
            LiveMoverReason::UnresolvedBlockers,
        );
    }

    #[test]
    fn no_evidence_has_no_live_mover() {
        let evidence = LiveMoverEvidence::default();

        assert!(!has_live_mover(&evidence));
        assert!(live_mover_reasons(&evidence).is_empty());
    }

    #[test]
    fn multiple_evidence_classes_return_stable_reason_list() {
        let evidence = LiveMoverEvidence {
            active_session: true,
            dispatch_inflight: true,
            open_pr: true,
            unresolved_blockers: true,
            ..Default::default()
        };

        assert!(has_live_mover(&evidence));
        assert_eq!(
            live_mover_reasons(&evidence),
            vec![
                LiveMoverReason::ActiveSession,
                LiveMoverReason::DispatchInflight,
                LiveMoverReason::OpenPr,
                LiveMoverReason::UnresolvedBlockers,
            ]
        );
    }

    // ── Productive always proceeds, regardless of count ─────────────────────

    #[test]
    fn productive_proceeds_at_zero() {
        assert_eq!(
            decide_run_disposition(RunProgress::Productive, 0, NUDGE_CAP),
            RunDisposition::Proceed
        );
    }

    #[test]
    fn productive_proceeds_even_past_cap() {
        // A productive run is never closed for budget reasons.
        assert_eq!(
            decide_run_disposition(RunProgress::Productive, 99, NUDGE_CAP),
            RunDisposition::Proceed
        );
    }

    // ── NoOp: nudge while under budget, then close ──────────────────────────

    #[test]
    fn noop_first_time_nudges() {
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 0, NUDGE_CAP),
            RunDisposition::Nudge
        );
    }

    #[test]
    fn noop_second_time_nudges() {
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 1, NUDGE_CAP),
            RunDisposition::Nudge
        );
    }

    #[test]
    fn noop_at_cap_closes() {
        // continuation_count == cap → budget exhausted → close.
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, NUDGE_CAP, NUDGE_CAP),
            RunDisposition::Close
        );
    }

    #[test]
    fn noop_over_cap_closes() {
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, NUDGE_CAP + 5, NUDGE_CAP),
            RunDisposition::Close
        );
    }

    /// The bound: with the production cap of 2, a no-op task is nudged at most
    /// twice (counts 0 and 1) and then closes (count 2+). Exactly two nudges.
    #[test]
    fn noop_is_bounded_to_exactly_two_nudges() {
        let nudges: Vec<RunDisposition> = (0..5)
            .map(|c| decide_run_disposition(RunProgress::NoOp, c, NUDGE_CAP))
            .collect();
        assert_eq!(
            nudges,
            vec![
                RunDisposition::Nudge, // count 0
                RunDisposition::Nudge, // count 1
                RunDisposition::Close, // count 2 (== cap)
                RunDisposition::Close, // count 3
                RunDisposition::Close, // count 4
            ]
        );
        let nudge_total = nudges
            .iter()
            .filter(|d| **d == RunDisposition::Nudge)
            .count();
        assert_eq!(
            nudge_total, NUDGE_CAP as usize,
            "must nudge exactly the cap"
        );
    }

    #[test]
    fn budget_park_noop_reuses_nudge_cap_ladder() {
        // A terminal budget park with no commits reaches the same no-op
        // disposition fork as any other empty completed run. There is no new
        // disposition variant: continuation_count 0 and 1 nudge, 2+ closes.
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 0, NUDGE_CAP),
            RunDisposition::Nudge
        );
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 1, NUDGE_CAP),
            RunDisposition::Nudge
        );
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 2, NUDGE_CAP),
            RunDisposition::Close
        );
    }

    // ── Inconclusive is never auto-nudged ───────────────────────────────────

    #[test]
    fn inconclusive_closes_even_with_budget() {
        // AC flipped but nothing physical — don't spend nudges on an ambiguous
        // signal; preserve the conservative close.
        assert_eq!(
            decide_run_disposition(RunProgress::Inconclusive, 0, NUDGE_CAP),
            RunDisposition::Close
        );
    }

    // ── Idempotency: the counter is the dedup key ───────────────────────────

    /// The same finished run can't be nudged twice: each nudge advances
    /// `continuation_count`, and re-feeding the *incremented* count yields the
    /// next step of the ladder, never a repeat of the same decision for the
    /// same count. This encodes the "(task, attempt) idempotency" invariant —
    /// the attempt is the continuation_count, monotonically advanced per nudge.
    #[test]
    fn each_count_maps_to_one_stable_decision() {
        for c in 0..10 {
            let a = decide_run_disposition(RunProgress::NoOp, c, NUDGE_CAP);
            let b = decide_run_disposition(RunProgress::NoOp, c, NUDGE_CAP);
            assert_eq!(a, b, "decision must be stable for a fixed count {c}");
        }
    }

    /// A cap of zero disables nudging entirely (immediate close) — a guard
    /// against a future caller passing 0 and silently looping.
    #[test]
    fn zero_cap_closes_immediately() {
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 0, 0),
            RunDisposition::Close
        );
    }
}
