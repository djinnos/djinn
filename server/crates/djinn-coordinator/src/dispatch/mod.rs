mod admission;
pub(crate) mod attempt_lifecycle;
/// Trigger B: the same-role cycling gate (threshold, prior-session terminal
/// disposition, and the arming decision). Re-exported through `crate::types`.
pub(crate) mod cycling_intervention;
pub(crate) mod lane_resolution_log;
pub(crate) mod liveness;
mod outcome;
/// Cumulative bound on the park rung's `no_attempted_remediation` decline, so a
/// guard whose counter cannot grow stops redispatching forever. Re-exported
/// through `crate::types`.
pub(crate) mod park_redispatch_bound;
pub(crate) mod post_intervention_lane;
/// Pre-dispatch respawn guard: consults attempt-history before fresh
/// spawn/admission and records guard-deferred audit rows.
pub(crate) mod respawn_guard;
/// Pure resume-source selector and candidate builder. Wired into the dispatch
/// path for re-dispatch after controlled terminations; returns `None` when
/// resume selection is disabled so default/off dispatch behavior is unchanged.
// Note: the public API is still exported for the follow-up worktree checkout
// task (twsk). The dispatcher uses the helpers directly to attach selection
// metadata to the session lifecycle path.
pub mod resume_source;
mod retry;
pub(crate) mod session_recovery;
pub(crate) mod task_dispatch;
/// Pre-pod-allocation warm build-cache freshness gate (ri23 Part 2). Reads warm
/// freshness facts, bounds the wait, and labels the dispatch decision without
/// owning any warm state.
pub(crate) mod warm_dispatch_gate;
pub(crate) mod wave_dispatch;

#[cfg(test)]
mod wave_dispatch_tests;

#[cfg(test)]
mod park_reason_tests;

#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use admission::model_under_user_cap;
// Crate-internal re-export so `test_helpers` can forward the lane half of the
// resident conjunction to the out-of-crate conformance target. Visibility of
// the function itself is unchanged: it stays `pub(crate)` in `admission`.
#[allow(unused_imports)]
pub(crate) use admission::lane_under_user_cap;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use admission::{
    DispatchCapObservation, DispatchCapObservationStage, observe_dispatch_cap_count,
    observe_dispatch_cap_counts, overlay_inflight_ledger, take_dispatch_cap_observations,
};
pub(crate) use outcome::DispatchOutcome;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use retry::PostInterventionHistory;
pub(crate) use retry::RemediationKind;

/// Threshold at which a task blocked by an all-candidates-open health breaker
/// gets its operator-visible activity entry. Test-only re-export so the
/// blameless-exhaustion regression suite asserts against the real constant.
#[cfg(test)]
pub(crate) use task_dispatch::BREAKER_OPEN_EXHAUSTION_SIGNAL_THRESHOLD;
