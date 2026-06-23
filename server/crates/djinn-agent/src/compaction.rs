//! Djinn-native conversation compaction — facade re-exports.
//!
//! The compaction implementation has been extracted to the `djinn-compaction`
//! crate.  This module preserves the existing `djinn_agent::compaction::*`
//! import path so callers need not change.

pub use djinn_compaction::{
    CompactionContext, compact_conversation, is_compaction_recoverable_error, needs_compaction,
};
