//! Djinn-native conversation compaction.
//!
//! When the accumulated input token count reaches 80% of the model's context
//! window, `compact_conversation` summarises the conversation via the LLM and
//! replaces the in-memory `Conversation` with a compact representation. The
//! original messages are persisted to the `session_messages` table before the
//! replacement so nothing is lost.

mod policy;
mod prompts;
mod summarizer;
mod truncate;

pub use policy::{compact_conversation, needs_compaction};
pub use prompts::CompactionContext;

/// Whether `err` is a failure that reactive compaction can recover from — a
/// context-window overflow or an orphaned tool-call/result reference. Both are
/// resolved by summarising the conversation and retrying, so any conversation
/// loop (worker reply loop *or* the chat handler) can gate a compact-and-retry
/// on this. Uses the provider-layer classifiers.
pub fn is_compaction_recoverable_error(err: &anyhow::Error) -> bool {
    djinn_provider::error_classify::is_context_length_error(err)
        || djinn_provider::error_classify::is_orphaned_tool_call_error(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::provider::error::ProviderError;

    #[test]
    fn recoverable_error_covers_overflow_and_orphaned_tool() {
        // Typed context overflow.
        assert!(is_compaction_recoverable_error(&anyhow::Error::new(
            ProviderError::ContextOverflow
        )));
        // Untyped context-length message (legacy path).
        assert!(is_compaction_recoverable_error(&anyhow::anyhow!(
            "400 prompt is too long: 210000 tokens > 200000 maximum"
        )));
        // Orphaned tool-call/result reference.
        assert!(is_compaction_recoverable_error(&anyhow::anyhow!(
            "No tool call found for function call output with id call_123"
        )));
        // Unrelated failures are NOT recoverable by compaction.
        assert!(!is_compaction_recoverable_error(&anyhow::Error::new(
            ProviderError::Authentication
        )));
        assert!(!is_compaction_recoverable_error(&anyhow::anyhow!(
            "connection reset by peer"
        )));
    }
}
