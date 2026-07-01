//! Coordinator-owned supervisor implementation modules.
//!
//! Contains the disposition layer (live-mover evidence, run-disposition
//! logic) and PR-open orchestration that the coordinator dispatch path
//! depends on.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::context::CoordinatorContext;
use djinn_provider::provider::LlmProvider;

pub mod disposition;
pub(crate) mod pr;
pub mod review_local_gates;

pub use disposition::{
    LiveMoverEvidence, LiveMoverReason, LiveMoverSummary, has_live_mover, live_mover_reasons,
    live_mover_summary, summarize_live_mover,
};
pub use pr::supervisor_pr_open;

/// Extra state captured when the coordinator dispatch wave builds the
/// closures that populate `SupervisorServices`.
///
/// Uses `CoordinatorContext` instead of `AgentContext` to avoid a
/// circular crate dependency.
#[derive(Clone)]
pub struct SupervisorCallbackContext {
    pub agent_context: CoordinatorContext,
    pub cancel: CancellationToken,
    /// Test seam: integration tests inject a stubbed `LlmProvider` so the
    /// stage can run end-to-end without a real vault credential.  Production
    /// callers leave this `None`.
    pub provider_override: Option<Arc<dyn LlmProvider>>,
}
