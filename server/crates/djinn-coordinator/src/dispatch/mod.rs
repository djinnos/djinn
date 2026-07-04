mod admission;
mod outcome;
/// Pure resume-source selector and candidate builder. Wired into the dispatch
/// path for re-dispatch after controlled terminations; returns `None` when
/// resume selection is disabled so default/off dispatch behavior is unchanged.
// Note: the public API is still exported for the follow-up worktree checkout
// task (twsk). The dispatcher uses the helpers directly to attach selection
// metadata to the session lifecycle path.
pub mod resume_source;
mod post_intervention_lane;
mod retry;
pub(crate) mod session_recovery;
mod task_dispatch;
mod wave_dispatch;

#[cfg(test)]
mod lifecycle_integration_tests;

pub(crate) use admission::model_under_user_cap;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use admission::{
    DispatchCapObservation, DispatchCapObservationStage, clear_dispatch_cap_observations,
    observe_dispatch_cap_count, observe_dispatch_cap_counts, overlay_inflight_ledger,
    take_dispatch_cap_observations,
};
pub(crate) use outcome::DispatchOutcome;
pub(crate) use retry::RemediationKind;
