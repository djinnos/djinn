mod admission;
pub(crate) mod attempt_lifecycle;
pub(crate) mod lane_resolution_log;
pub(crate) mod liveness;
mod outcome;
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
mod task_dispatch;
/// Pre-pod-allocation warm build-cache freshness gate (ri23 Part 2). Reads warm
/// freshness facts, bounds the wait, and labels the dispatch decision without
/// owning any warm state.
pub(crate) mod warm_dispatch_gate;
mod wave_dispatch;

#[cfg(test)]
mod wave_dispatch_tests;

#[cfg(test)]
mod lifecycle_integration_tests;

#[cfg(test)]
mod park_reason_tests;

pub(crate) use admission::model_under_user_cap;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use admission::{
    DispatchCapObservation, DispatchCapObservationStage, clear_dispatch_cap_observations,
    observe_dispatch_cap_count, observe_dispatch_cap_counts, overlay_inflight_ledger,
    take_dispatch_cap_observations,
};
pub(crate) use outcome::DispatchOutcome;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use retry::PostInterventionHistory;
pub(crate) use retry::RemediationKind;
