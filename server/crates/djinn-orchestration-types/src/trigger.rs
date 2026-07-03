//! Narrow trait for triggering a coordinator dispatch pass.
//!
//! The slot pool uses this trait (via [`CoordinatorTrigger::try_trigger_dispatch`])
//! to avoid a direct dependency on the full `CoordinatorHandle` and its
//! internal message/actor types.

/// Fire-and-forget dispatch trigger.
///
/// Implementors must be cheap to call — the canonical implementation
/// (`CoordinatorHandle::try_trigger_dispatch`) uses `mpsc::try_send` and
/// never blocks.
pub trait CoordinatorTrigger: Send + Sync {
    /// Best-effort, non-blocking dispatch trigger.
    fn try_trigger_dispatch(&self);

    /// Route a second-strike no-progress submission through the coordinator's
    /// planner intervention machinery. This does NOT increment the
    /// dispatch_failure_streak — it enters the same path as a loop-guard
    /// trip (clear streak/backoff, dispatch Planner escalation).
    ///
    /// Implementations that do not support coordinator routing (e.g. test
    /// stubs) may no-op.
    fn try_route_no_progress_intervention(&self, _task_id: &str, _reason: &str) {
        // Default no-op for implementations that don't support intervention routing.
    }
}
