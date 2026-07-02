mod admission;
mod outcome;
/// Pure resume-source selector; not wired into the dispatch path yet (see task
/// 9tun). The public API is consumed by tests today and will be integrated by a
/// later task, so dead_code is expected until then.
pub(crate) mod resume_source;
mod retry;
pub(crate) mod session_recovery;
mod task_dispatch;
mod wave_dispatch;

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
