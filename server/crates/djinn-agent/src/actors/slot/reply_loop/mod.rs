//! Djinn-native reply loop.
//!
//! The reply loop drives an `LlmProvider` stream, dispatches tool calls via the
//! extension layer, and continues until the assistant produces a text-only
//! response or a termination condition is reached.
//!
//! The original `slot/reply_loop.rs` (2,064 lines) has been decomposed into
//! focused submodules.  The previously-partial split already extracted
//! `error_handling`, `streaming`, and `tool_dispatch`; this final layout adds
//! `turn` (the main `run_reply_loop` orchestrator and its helpers), `persistence`
//! (the session-message persistence + serialization helpers), and `tests` (the
//! integration test suite).  The public surface (`ReplyLoopContext` and
//! `run_reply_loop`) is re-exported here so the external path
//! `crate::actors::slot::reply_loop::{ReplyLoopContext, run_reply_loop}` keeps
//! resolving identically.

pub(crate) mod budget;
pub(crate) mod error_handling;
pub(crate) mod loop_guard;
mod persistence;
mod streaming;
mod tool_dispatch;
mod turn;

pub(crate) use turn::{ReplyLoopContext, run_reply_loop};

#[cfg(test)]
mod tests;
