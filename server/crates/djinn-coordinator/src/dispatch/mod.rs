mod admission;
mod outcome;
#[allow(clippy::disallowed_methods)]
// dispatch retry tests/telemetry fixtures use real monotonic instants
mod retry;
pub(crate) mod session_recovery;
#[allow(clippy::disallowed_methods)]
// dispatcher cooldown/deadline bookkeeping still uses monotonic instants
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
