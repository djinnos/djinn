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
}
