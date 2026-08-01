//! Trigger B — the same-role cycling gate.
//!
//! Extracted from `crate::types` (which sits close to the guard's byte ceiling)
//! so the whole trigger-B arming decision — its threshold, the prior session's
//! terminal disposition, and the gate itself — lives in one place next to its
//! only production caller in [`super::task_dispatch`]. `crate::types`
//! re-exports all three names, so every existing `use super::*` call site is
//! unchanged.

/// Consecutive same-role failed dispatch attempts after which the task is
/// routed to a Planner intervention (trigger B) instead of only riding the
/// cooldown ladder toward [`crate::types::MAX_DISPATCH_FAILURES`].
///
/// Trigger A (`reopen_count >= REOPEN_INTERVENTION_THRESHOLD`) only sees loops
/// that pass through `open`: a reviewer rejection reopens the task and bumps
/// the count. A review-cycle livelock never reopens — the task bounces
/// `needs_task_review → in_progress → needs_task_review`, each cycle a
/// same-role reappearance, `reopen_count` pinned at 0 — so the only
/// bound was the terminal close at streak 10, which force-closes a task whose
/// worker output may be perfectly fine (the t9wi/32bk wedge, 2026-06-11:
/// streaks of 7-8 with 30-min cooldowns while the reviewer stage never ran).
/// Trigger B hands that loop to the Planner for the same decompose / rescope /
/// close decision the reopen path gets, well before the terminal close.
///
/// Rationale for `4`: the original attempt plus three same-role cycles — with
/// the ladder that already spans well over an hour of wall clock, comparable
/// depth to the reopen threshold of 3 — without firing on a one-off pod kill
/// or a transient infra blip that the first cooldown rungs absorb.
pub(crate) const STREAK_INTERVENTION_THRESHOLD: u32 = 4;

/// What the prior same-role run's session actually did, as proved by the
/// terminal `task_attempts` row the dispatch pass reads back
/// (`CoordinatorActor::latest_attempt_strike_decision`).
///
/// This is a statement about the SESSION's terminal disposition, never about
/// wall-clock duration and never about the task's status: a 10-minute run and a
/// 10-second run are indistinguishable here, and both `open` and
/// `needs_task_review` loops can contain either kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorSessionDisposition {
    /// The run reached its own terminal decision — it submitted, completed,
    /// was reopened by a reviewer, or crashed mid-run. Trigger B's premise
    /// ("each run finishes and the task lands right back where it was") holds
    /// for this reappearance.
    Concluded,
    /// The session was cancelled, reclaimed, or never created at all, so the
    /// run never reached its own terminal decision (attempt outcome
    /// `cancelled` / `timed_out` / `interrupted` / `spawn_failed` — see
    /// [`TaskAttemptOutcome::never_concluded`](djinn_core::models::task_attempt::TaskAttemptOutcome::never_concluded)).
    NeverConcluded,
}

impl PriorSessionDisposition {
    /// Classify a `task_attempts.outcome` string. Unknown/unparseable outcomes
    /// fail CLOSED to [`Self::Concluded`] so a storage or vocabulary drift can
    /// never silently disarm trigger B for a genuine review-cycle livelock.
    pub(crate) fn from_attempt_outcome(outcome: &str) -> Self {
        match outcome.parse::<djinn_core::models::task_attempt::TaskAttemptOutcome>() {
            Ok(parsed) if parsed.never_concluded() => Self::NeverConcluded,
            _ => Self::Concluded,
        }
    }
}

/// Trigger-B gate: should a same-role failure streak route this dispatch to a
/// Planner intervention?
///
/// Only "the run keeps completing but the task never moves" loops qualify:
///
/// - `had_provider_failure` excludes reappearances explained by a typed
///   provider fault (throttle, auth, server error) — those are provider
///   problems, already bounded by the breaker/failover and the cooldown
///   ladder, not task-shape problems a Planner can fix. Trigger D
///   (`maybe_escalate_provider_failure_streak`) owns that class.
/// - `prior_session` excludes reappearances whose prior session was CANCELLED,
///   reclaimed, or never created before the run could conclude. Trigger B
///   asserts, in its own user-facing reason, that "each run finishes and the
///   task lands right back where it was" — an infrastructure cancellation makes
///   that false, and it is no evidence whatsoever that the task's scope or
///   acceptance criteria are wrong. Task 7mq0 (2026-07-28) rode four
///   consecutive `session cancelled` + coordinator-recovery cycles into this
///   gate; the Planner it summoned had no lever but reshaping the work, so an
///   infra timeout permanently narrowed a healthy task. On 2026-08-01 a
///   launcher protocol mismatch (`resize-v2` catalog vs `leaf-v1` server) made
///   `resolve_dispatch_image` refuse every dispatch at `runtime.prepare`, and
///   four `spawn_failed` streaks minted four planner remediations for tasks
///   that had never been given a turn — a `spawn_failed` session was never
///   created at all, which is strictly LESS evidence of task-level
///   non-convergence than an `interrupted` run that at least started.
///   Coordinator stall kills keep their own truthful escalation
///   (`STALL_CANCEL_ESCALATION_THRESHOLD` in `session_recovery`).
/// - worker/reviewer roles only: a planner-role task must never escalate to
///   another Planner (recursive intervention), and lead/architect loops have
///   their own routing.
///
/// The genuine review-cycle livelock this trigger exists to catch (the
/// t9wi/32bk wedge) is untouched: those runs terminalize their attempts
/// `completed` / `submitted` / `reopened`, which classify as
/// [`PriorSessionDisposition::Concluded`].
///
/// MIXED STREAKS: `prior_session` describes the MOST RECENT same-role attempt
/// only (`CoordinatorActor::latest_attempt_strike_decision` reads the newest
/// non-guard row). A streak that mixes infra and concluded outcomes therefore
/// arms iff its newest attempt concluded — unchanged from how the
/// cancelled/timed_out/interrupted exclusion has always behaved, and the reason
/// text emitted by `maybe_intervene_on_cycling_task` says exactly that.
pub(crate) fn should_route_cycling_intervention(
    role: &str,
    next_streak: u32,
    had_provider_failure: bool,
    prior_session: PriorSessionDisposition,
) -> bool {
    !had_provider_failure
        && prior_session == PriorSessionDisposition::Concluded
        && next_streak >= STREAK_INTERVENTION_THRESHOLD
        && matches!(role, "worker" | "reviewer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MAX_DISPATCH_FAILURES;

    #[test]
    fn trigger_b_gate_arms_on_clean_same_role_streak_only() {
        use PriorSessionDisposition::{Concluded, NeverConcluded};

        // The review-cycle livelock shape: reviewer-role reappearances with no
        // provider failure, streak at the threshold → arm the Planner route.
        assert!(
            should_route_cycling_intervention(
                "reviewer",
                STREAK_INTERVENTION_THRESHOLD,
                false,
                Concluded
            ),
            "a clean reviewer streak at the threshold must arm trigger B \
             (the t9wi/32bk review-cycle bounce)"
        );
        assert!(should_route_cycling_intervention(
            "worker",
            STREAK_INTERVENTION_THRESHOLD + 3,
            false,
            Concluded
        ));

        // Below the threshold: the ordinary ladder rungs absorb one-off pod
        // kills / infra blips without burning a Planner session.
        assert!(!should_route_cycling_intervention(
            "reviewer",
            STREAK_INTERVENTION_THRESHOLD - 1,
            false,
            Concluded
        ));

        // A typed provider failure explains the reappearance — that's a
        // provider problem (breaker/failover territory), not a task-shape
        // problem a Planner can fix.
        assert!(!should_route_cycling_intervention(
            "reviewer",
            STREAK_INTERVENTION_THRESHOLD,
            true,
            Concluded
        ));

        // A cancelled / reclaimed / never-created session never finished, so the
        // reappearance says nothing about the task's shape either (task 7mq0,
        // and the 2026-08-01 `spawn_failed` remediation storm).
        assert!(!should_route_cycling_intervention(
            "worker",
            STREAK_INTERVENTION_THRESHOLD,
            false,
            NeverConcluded
        ));
        assert!(!should_route_cycling_intervention(
            "worker",
            STREAK_INTERVENTION_THRESHOLD + 6,
            false,
            NeverConcluded
        ));

        // Planner-role tasks must never recursively escalate to a Planner.
        assert!(!should_route_cycling_intervention(
            "planner",
            STREAK_INTERVENTION_THRESHOLD,
            false,
            Concluded
        ));

        // Trigger B must arm strictly before the terminal close so the
        // Planner gets its pass before a force-close can fire.
        const {
            assert!(
                STREAK_INTERVENTION_THRESHOLD < MAX_DISPATCH_FAILURES,
                "intervention must precede the terminal-close backstop"
            );
        }
    }

    /// The disposition is derived from the session's terminal `task_attempts`
    /// outcome — the only signal the gate is allowed to use. A run that reached
    /// its own terminal decision (including a genuine mid-run crash) stays
    /// `Concluded`; cancellation, reclamation, and a session that was never
    /// created (`spawn_failed`) disarm trigger B. An unknown outcome string
    /// fails CLOSED so vocabulary drift cannot silently disable the escalation.
    #[test]
    fn prior_session_disposition_excludes_every_never_concluded_outcome() {
        for outcome in [
            "cancelled",    // session cancellation token fired (task 7mq0)
            "timed_out",    // coordinator stall kill reclaimed a live session
            "interrupted",  // deploy/rollout/reap environmental interruption
            "spawn_failed", // no session ever created (2026-08-01 protocol mismatch)
        ] {
            assert_eq!(
                PriorSessionDisposition::from_attempt_outcome(outcome),
                PriorSessionDisposition::NeverConcluded,
                "`{outcome}` must not count as a concluded run"
            );
        }

        for outcome in [
            "completed",
            "submitted",
            "reopened",
            "force_closed",
            "loop_guard_tripped",
            "crashed",
            "pending",
            "not_a_real_outcome",
        ] {
            assert_eq!(
                PriorSessionDisposition::from_attempt_outcome(outcome),
                PriorSessionDisposition::Concluded,
                "`{outcome}` must keep trigger B armed"
            );
        }
    }
}
