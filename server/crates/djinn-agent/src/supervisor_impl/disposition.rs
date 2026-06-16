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

/// Combined live-mover answer: a boolean plus the explicit reason list, in one
/// value non-PR callers can pass around without losing the explanation.
///
/// This is the **non-PR internal API** the live-mover epic exposes for
/// post-run and orphan-task checks (e.g. the future `vnwi` doctor orphan-task
/// check). It deliberately does not collapse to a bare `bool` — callers can
/// always project the boolean via [`LiveMoverSummary::is_live`] but the
/// structured reasons are preserved so consumers can explain *why* a task was
/// considered live (e.g. in operator logs, doctor reports, or audit trails).
///
/// Construction is pure: a function of the supplied [`LiveMoverEvidence`]
/// alone. The evidence itself is normally built by
/// `actors::coordinator::CoordinatorActor::collect_live_mover_evidence`
/// (epic task `yc6g`), but **this type is independent of that path** —
/// callers that already hold hard evidence (tests, alternative collectors,
/// mocks) can feed it in directly.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveMoverSummary {
    /// Convenience boolean mirror of the [`LiveMoverReason`] set: true when
    /// any evidence class says the task still has a live mover. Equivalent
    /// to [`has_live_mover`].
    pub has_live_mover: bool,
    /// Stable, ordered list of every evidence class that contributed to
    /// `has_live_mover = true`. Empty when no evidence is present. Order
    /// follows the field order of [`LiveMoverEvidence`] and is documented on
    /// [`live_mover_reasons`].
    pub reasons: Vec<LiveMoverReason>,
}

impl LiveMoverSummary {
    /// Borrow the boolean as `&self` for ergonomic call sites that want a
    /// plain `bool` (e.g. `if summary.is_live() { ... }`).
    #[allow(dead_code)]
    pub fn is_live(&self) -> bool {
        self.has_live_mover
    }
}

/// Build a [`LiveMoverSummary`] from already-collected evidence.
///
/// This is the **canonical non-PR entry point** for callers that want both a
/// boolean and the explanatory reason list. It does not import
/// `supervisor_impl::pr` and does not touch the no-op disposition ladder —
/// those concerns are deliberately separate. A future `vnwi` doctor
/// orphan-task check, a post-run audit, or a coordinator-side diagnostics
/// surface can call this and obtain a structured answer without coupling to
/// the PR-open code path.
///
/// Pure: a function of the supplied evidence alone. No I/O, no DB, no
/// GitHub, no LLM judgement.
#[allow(dead_code)]
pub(crate) fn live_mover_summary(evidence: &LiveMoverEvidence) -> LiveMoverSummary {
    let reasons = live_mover_reasons(evidence);
    LiveMoverSummary {
        has_live_mover: has_live_mover(evidence),
        reasons,
    }
}

/// Convenience: build a [`LiveMoverSummary`] for a task, given pre-collected
/// evidence. This is the surface non-PR callers reach for when they already
/// have a [`LiveMoverEvidence`] (e.g. returned by
/// `CoordinatorActor::collect_live_mover_evidence`) and just want the
/// combined boolean + reasons.
///
/// It is a thin named wrapper around [`live_mover_summary`] that documents
/// the canonical "ask the live-mover predicate" call site. Returning a
/// [`LiveMoverSummary`] (not a bare `bool`) means callers can always inspect
/// the reasons — see [`LiveMoverSummary::is_live`] for the boolean shortcut.
#[allow(dead_code)]
pub(crate) fn summarize_live_mover(evidence: &LiveMoverEvidence) -> LiveMoverSummary {
    live_mover_summary(evidence)
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
        assert_eq!(NUDGE_CAP, 2, "the production no-op nudge cap is locked");
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 0, NUDGE_CAP),
            RunDisposition::Nudge
        );
    }

    #[test]
    fn noop_disposition_semantics_are_locked() {
        assert_eq!(NUDGE_CAP, 2, "do not change the no-op nudge budget");
        assert_eq!(
            decide_run_disposition(RunProgress::Productive, 0, NUDGE_CAP),
            RunDisposition::Proceed,
            "productive runs must continue to proceed"
        );
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 0, NUDGE_CAP),
            RunDisposition::Nudge,
            "first no-op still nudges"
        );
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 1, NUDGE_CAP),
            RunDisposition::Nudge,
            "second no-op still nudges"
        );
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 2, NUDGE_CAP),
            RunDisposition::Close,
            "at the cap, no-op closes"
        );
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, 3, NUDGE_CAP),
            RunDisposition::Close,
            "over the cap, no-op closes"
        );
        assert_eq!(
            decide_run_disposition(RunProgress::Inconclusive, 0, NUDGE_CAP),
            RunDisposition::Close,
            "inconclusive remains a conservative close"
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

    // ── LiveMoverSummary: non-PR reusable internal API ──────────────────────

    /// The reusable summary must agree with the underlying pure predicate.
    #[test]
    fn live_mover_summary_agrees_with_has_live_mover_predicate() {
        let cases = [
            LiveMoverEvidence::default(),
            LiveMoverEvidence {
                active_session: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                open_pr: true,
                pr_poller_owned: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                unresolved_blockers: true,
                ..Default::default()
            },
        ];
        for evidence in cases.iter() {
            let summary = live_mover_summary(evidence);
            assert_eq!(
                summary.has_live_mover,
                has_live_mover(evidence),
                "summary.has_live_mover must match has_live_mover(&evidence)"
            );
            assert_eq!(
                summary.is_live(),
                has_live_mover(evidence),
                "summary.is_live() must match has_live_mover(&evidence)"
            );
            assert_eq!(
                summary.reasons,
                live_mover_reasons(evidence),
                "summary.reasons must match live_mover_reasons(&evidence)"
            );
        }
    }

    /// Empty evidence must produce a `not live` summary with no reasons — the
    /// canonical "no mover" answer a future vnwi doctor orphan-task check will
    /// consult when deciding to flag a task as stuck.
    #[test]
    fn live_mover_summary_empty_is_not_live() {
        let summary = live_mover_summary(&LiveMoverEvidence::default());
        assert!(!summary.has_live_mover);
        assert!(!summary.is_live());
        assert!(summary.reasons.is_empty());
    }

    /// `summarize_live_mover` is a documented non-PR entry point and must
    /// return the same value as `live_mover_summary` for any evidence input.
    /// This pins the contract that downstream non-PR callers can reach for
    /// either name and get an equivalent result.
    #[test]
    fn summarize_live_mover_matches_live_mover_summary() {
        let evidence = LiveMoverEvidence {
            active_session: true,
            review_pending_with_reviewer: true,
            unresolved_blockers: true,
            ..Default::default()
        };
        assert_eq!(
            summarize_live_mover(&evidence),
            live_mover_summary(&evidence)
        );
    }

    /// The summary is a deterministic view of the evidence: same evidence in
    /// ⇒ same summary out. This is the property an audit trail / operator
    /// log / doctor report relies on to reproduce the predicate output.
    #[test]
    fn live_mover_summary_is_deterministic() {
        let evidence = LiveMoverEvidence {
            active_session: true,
            dispatch_inflight: true,
            open_pr: true,
            unresolved_blockers: true,
            ..Default::default()
        };
        let a = live_mover_summary(&evidence);
        let b = live_mover_summary(&evidence);
        assert_eq!(a, b);
        assert_eq!(
            a.reasons,
            vec![
                LiveMoverReason::ActiveSession,
                LiveMoverReason::DispatchInflight,
                LiveMoverReason::OpenPr,
                LiveMoverReason::UnresolvedBlockers,
            ]
        );
    }

    /// Compile-time witness that the new public-to-crate surface is reachable
    /// via `supervisor_impl` (not via `supervisor_impl::pr`). This is the
    /// "callable without importing PR-open-specific code" guarantee the
    /// epic-level acceptance criteria require.
    #[test]
    fn summary_api_is_reachable_via_supervisor_impl_module_root() {
        use crate::supervisor_impl::{
            LiveMoverEvidence as RootEvidence, LiveMoverSummary as RootSummary,
            live_mover_summary as root_summarize,
        };

        // Build a synthetic piece of evidence purely from the supervisor_impl
        // re-exports — no `supervisor_impl::pr` symbol in scope.
        let evidence = RootEvidence {
            active_session: true,
            ..Default::default()
        };
        let summary: RootSummary = root_summarize(&evidence);
        assert!(summary.has_live_mover);
        assert_eq!(summary.reasons, vec![LiveMoverReason::ActiveSession]);
    }
}
