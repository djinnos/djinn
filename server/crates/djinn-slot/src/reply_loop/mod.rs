//! Djinn-native reply loop.
//!
//! The reply loop drives an `LlmProvider` stream, dispatches tool calls via the
//! extension layer, and continues until the assistant produces a text-only
//! response or a termination condition is reached.
//!
//! This module owns the canonical reply-loop implementation for `djinn-slot`.
//! Host-only tool dispatch, output-stash, and MCP operations are behind the
//! [`crate::host::SlotToolDispatcher`] trait; compaction uses `djinn-compaction`
//! directly; and `djinn-agent` is never imported.

#[cfg(any(test, feature = "test-support"))]
pub mod budget;
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) mod budget;
pub mod compaction_guard;
pub mod error_handling;
pub mod loop_guard;
#[cfg(any(test, feature = "test-support"))]
pub mod persistence;
#[cfg(not(any(test, feature = "test-support")))]
mod persistence;
pub mod phase;
#[cfg(any(test, feature = "test-support"))]
pub mod streaming;
#[cfg(not(any(test, feature = "test-support")))]
mod streaming;
mod tool_dispatch;
#[cfg(any(test, feature = "test-support"))]
pub mod turn;
#[cfg(not(any(test, feature = "test-support")))]
mod turn;
mod turn_budget;

pub use compaction_guard::CompactionCriticalSection;
pub use phase::{SessionPhase, SessionPhaseRole, SessionPhaseTracker};
pub use turn::{ReplyLoopContext, run_reply_loop};

// Soft-budget tests hold `SESSION_BUDGET_ENV_LOCK` across `.await` on
// purpose: the lock serializes env-var mutation (set/remove) for the
// duration of each async test so concurrent tests cannot race the shared
// process env. Mirrors the `AUTO_CODE_CONTEXT_ENV_LOCK` pattern in
// `helpers/tests.rs`.
#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests;
