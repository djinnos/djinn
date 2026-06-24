//! Supervisor disposition/live-mover evaluation surface and PR-open body.
//!
//! Moved from `djinn-agent::supervisor_impl`. Contains the pure
//! disposition logic, live-mover evidence model, and the PR-open body.
//! The stage executor remains in `djinn-agent`.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use djinn_provider::provider::LlmProvider;

pub mod disposition;
pub(crate) mod pr;
#[cfg(test)]
mod pr_close;

pub use disposition::{
    LiveMoverEvidence, LiveMoverReason, LiveMoverSummary, has_live_mover, live_mover_reasons,
    live_mover_summary, summarize_live_mover,
};

pub(crate) use pr::supervisor_pr_open;

/// Extra state captured when building supervisor closures.
#[derive(Clone)]
pub(crate) struct SupervisorCallbackContext {
    pub agent_context: AgentContext,
    pub cancel: CancellationToken,
    pub provider_override: Option<Arc<dyn LlmProvider>>,
}
