#[cfg(any(test, feature = "test-support"))]
use std::sync::Arc;

#[cfg(any(test, feature = "test-support"))]
use tokio::sync::mpsc;

#[cfg(any(test, feature = "test-support"))]
use crate::context::AgentContext;

#[cfg(any(test, feature = "test-support"))]
use super::super::SlotHandle;

pub use djinn_slot::{ModelPoolStatus, PoolError, PoolMessage, PoolStatus, RunningTaskInfo};

/// Agent-facade test factory retained for callers that construct slots with an
/// `AgentContext`. Production pool behavior and all pool message/status types
/// are canonical `djinn-slot` exports.
#[cfg(any(test, feature = "test-support"))]
pub type SlotFactory = Arc<
    dyn Fn(
            usize,
            String,
            mpsc::Sender<super::super::SlotEvent>,
            AgentContext,
            tokio_util::sync::CancellationToken,
        ) -> SlotHandle
        + Send
        + Sync,
>;
