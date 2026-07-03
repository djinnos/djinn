use crate::context::AgentContext;
use djinn_supervisor::SupervisorServices;

use super::adapter::{build_slot_context, AgentHostCallbacks};

/// Build a dispatch-pathway [`djinn_slot::host::SlotContext`] from an [`AgentContext`].
pub(crate) fn agent_to_dispatch_slot_context(
    agent: &AgentContext,
) -> djinn_slot::host::SlotContext {
    build_slot_context(
        agent,
        std::sync::Arc::new(AgentHostCallbacks::dispatch(agent)),
        None,
    )
}

/// Build a reply-loop [`djinn_slot::host::SlotContext`] that routes liveness
/// and token-flush heartbeats through the live [`SupervisorServices`] handle.
pub(crate) fn agent_to_reply_loop_slot_context(
    agent: &AgentContext,
    services: &dyn SupervisorServices,
) -> djinn_slot::host::SlotContext {
    // SAFETY: the reply loop awaits every callback future before returning.
    let services_static = unsafe {
        std::mem::transmute::<&dyn SupervisorServices, &'static dyn SupervisorServices>(services)
    };
    build_slot_context(
        agent,
        std::sync::Arc::new(AgentHostCallbacks::reply_loop(agent, services_static)),
        None,
    )
}
